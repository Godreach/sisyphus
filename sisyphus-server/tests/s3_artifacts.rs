//! 票 #123：单文件产物经临时对象安全发布到 S3。
//!
//! 观测缝：Agent 预签名 PUT（临时 key）→ complete 校验复制 → 构建详情
//! viewer 获短期 GET URL；pending 完成前不可见。Mock S3 进程内 axum。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use common::{DEFAULT_PEER, custom_req};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use sisyphus_model::pipeline::{Job, Pipeline, Revision, Stage};
use sisyphus_model::validate::BuildSnapshot;
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::config::S3Config;
use sisyphus_server::storage::prepare_s3;
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::store::builds::{BuildRepo, BuildRow, StartBuild, TriggerSource};
use sisyphus_server::store::jobs::{JobRepo, NewJob};
use sisyphus_server::store::projects::{NewProject, ProjectRepo, ScmType};
use sisyphus_server::{api, store};

#[derive(Clone)]
struct Mock {
    access_key: String,
    bucket: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

async fn handle(State(mock): State<Mock>, req: Request<axum::body::Body>) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let presigned = query.contains("X-Amz-Algorithm") || query.contains("x-amz-algorithm");
    if !presigned && !auth.contains(&format!("Credential={}/", mock.access_key)) {
        return (
            StatusCode::FORBIDDEN,
            "<Error><Code>InvalidAccessKeyId</Code><Message>denied</Message></Error>",
        )
            .into_response();
    }
    let body = req
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();

    let Some(rest) = path.strip_prefix(&format!("/{}", mock.bucket)) else {
        return (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchBucket</Code><Message>missing</Message></Error>",
        )
            .into_response();
    };
    let key = rest.trim_start_matches('/');

    if key.is_empty() && (method == axum::http::Method::HEAD || method == axum::http::Method::GET) {
        return StatusCode::OK.into_response();
    }

    match method {
        m if m == axum::http::Method::PUT => {
            if let Some(src) = headers
                .get("x-amz-copy-source")
                .and_then(|v| v.to_str().ok())
            {
                let src_key = copy_source_key(src, &mock.bucket);
                let data = mock
                    .objects
                    .lock()
                    .unwrap()
                    .get(&src_key)
                    .cloned()
                    .unwrap_or_default();
                mock.objects.lock().unwrap().insert(key.to_string(), data);
                return StatusCode::OK.into_response();
            }
            mock.objects
                .lock()
                .unwrap()
                .insert(key.to_string(), body.to_vec());
            StatusCode::OK.into_response()
        }
        m if m == axum::http::Method::HEAD => {
            let objects = mock.objects.lock().unwrap();
            if let Some(data) = objects.get(key) {
                let mut resp = StatusCode::OK.into_response();
                resp.headers_mut()
                    .insert("content-length", data.len().to_string().parse().unwrap());
                resp
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        m if m == axum::http::Method::GET => {
            let Some(data) = mock.objects.lock().unwrap().get(key).cloned() else {
                return StatusCode::NOT_FOUND.into_response();
            };
            if let Some(range) = headers.get("range").and_then(|v| v.to_str().ok())
                && let Some(rest) = range.strip_prefix("bytes=")
            {
                let mut parts = rest.split('-');
                let start: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
                let end: usize = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(data.len().saturating_sub(1));
                let slice = data
                    .get(start..=end.min(data.len().saturating_sub(1)))
                    .unwrap_or(&[]);
                return (StatusCode::PARTIAL_CONTENT, slice.to_vec()).into_response();
            }
            (StatusCode::OK, data).into_response()
        }
        m if m == axum::http::Method::DELETE => {
            mock.objects.lock().unwrap().remove(key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn copy_source_key(src: &str, bucket: &str) -> String {
    let src = src.trim_start_matches('/');
    let prefix = format!("{bucket}/");
    src.strip_prefix(&prefix).unwrap_or(src).to_string()
}

async fn spawn_mock(access_key: &str, bucket: &str) -> (SocketAddr, Mock) {
    let mock = Mock {
        access_key: access_key.into(),
        bucket: bucket.into(),
        objects: Arc::new(Mutex::new(HashMap::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock s3");
    let addr = listener.local_addr().expect("addr");
    let app = Router::new().fallback(any(handle)).with_state(mock.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock serve");
    });
    (addr, mock)
}

fn s3_cfg(addr: SocketAddr, bucket: &str, key: &str, secret: &str) -> S3Config {
    S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: "prod".into(),
        access_key_id: key.into(),
        secret_access_key: secret.into(),
        path_style: true,
        ca_path: None,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    app: common::TestApp,
    cookie: String,
    agent_token: String,
    build: BuildRow,
    job_a: i64,
    job_b: i64,
    mock: Mock,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let (addr, mock) = spawn_mock("good-key", "sisy").await;
    let cfg = s3_cfg(addr, "sisy", "good-key", "secret");
    let client = prepare_s3(&pool, Some(&cfg))
        .await
        .expect("S3 启动校验")
        .expect("已配置");
    let state = api::AppState::new(
        pool.clone(),
        dir.path().to_path_buf(),
        false,
        master_key,
        sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES,
        sisyphus_server::config::DEFAULT_RETENTION_DAYS,
        sisyphus_server::config::DEFAULT_METRICS_AUTH,
    )
    .await
    .expect("装配 AppState")
    .with_s3(Some(client));
    let app = common::test_app_from_state(state.clone(), dir.path());

    let project = ProjectRepo::new(pool.clone())
        .create(NewProject {
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        })
        .await
        .expect("建项目");
    let agent_token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let agent = state
        .agents
        .create(NewAgent {
            name: "linux-1".into(),
            token_hash: token_hash(&agent_token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建 Agent 条目");

    let job_def = |name: &str, uploads: Vec<(&str, &str)>| Job {
        name: name.into(),
        exec_env: None,
        labels: vec![],
        when: None,
        env: vec![],
        allow_failure: false,
        retry_count: 0,
        timeout_minutes: 0,
        artifact_uploads: uploads
            .into_iter()
            .map(|(n, p)| sisyphus_model::pipeline::ArtifactUpload {
                name: n.into(),
                path: p.into(),
            })
            .collect(),
        artifact_downloads: vec![],
        caches: vec![],
        secrets: vec![],
        steps: vec![],
    };
    let snapshot = BuildSnapshot::new(
        Pipeline {
            name: "release".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "main".into(),
                when: None,
                jobs: vec![
                    job_def("build", vec![("dist.bin", "dist.bin")]),
                    job_def("package", vec![]),
                ],
            }],
            revision: None,
        },
        Revision {
            number: 1,
            operator: "tester".into(),
            at_ms: 0,
        },
    );
    let build = BuildRepo::new(pool.clone())
        .start(StartBuild {
            project_id: project.id,
            pipeline_name: "release".into(),
            trigger: TriggerSource::Manual,
            trigger_detail: "{}".into(),
            snapshot,
        })
        .await
        .expect("建构建");
    let spec_a = r#"{"artifact_uploads":[{"name":"dist.bin","path":"dist.bin"}]}"#;
    let job_a = JobRepo::new(pool.clone())
        .insert(NewJob {
            build_id: build.id,
            stage_index: 0,
            name: "build".into(),
            attempt: 1,
            spec_json: Some(spec_a.into()),
            agent_id: Some(agent.id),
            labels: vec![],
            timeout_minutes: 0,
            retry_count: 0,
            allow_failure: false,
        })
        .await
        .expect("建任务 A");
    let job_b = JobRepo::new(pool.clone())
        .insert(NewJob {
            build_id: build.id,
            stage_index: 0,
            name: "package".into(),
            attempt: 1,
            spec_json: Some(r#"{"artifact_uploads":[]}"#.into()),
            agent_id: Some(agent.id),
            labels: vec![],
            timeout_minutes: 0,
            retry_count: 0,
            allow_failure: false,
        })
        .await
        .expect("建任务 B");

    let cookie = common::setup_and_login(&app).await;
    Harness {
        _dir: dir,
        app,
        cookie,
        agent_token,
        build,
        job_a: job_a.id,
        job_b: job_b.id,
        mock,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn agent_post_json(h: &Harness, path: &str, body: &str) -> axum::response::Response {
    custom_req(
        &h.app,
        "POST",
        path,
        Some(body.into()),
        None,
        &[("authorization", format!("Bearer {}", h.agent_token))],
        DEFAULT_PEER,
    )
    .await
}

async fn viewer_get(h: &Harness, path: &str) -> axum::response::Response {
    common::req_with_cookie(&h.app, "GET", path, None, Some(&h.cookie)).await
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// 票 #123 主路径：声明内小文件经临时 key 直传，Server 校验复制后才 ready，
/// viewer 拿短期下载 URL；完成前不可见，最终 key 从未出现在 PUT URL 里。
#[tokio::test]
async fn s3_single_file_is_ready_only_after_temp_copy() {
    let h = harness().await;
    let bytes = b"s3-artifact-payload";
    let digest = sha256_hex(bytes);
    let grant_path = format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a);

    let resp = agent_post_json(&h, &grant_path, "{}").await;
    assert_eq!(resp.status(), 200, "签发上传 URL 应 200");
    let body = common::body_json(resp).await;
    let url = body["url"].as_str().expect("url");
    assert!(
        url.contains("/artifacts/tmp/"),
        "PUT URL 必须指向临时 key：{url}"
    );
    assert!(
        !url.contains("/artifacts/final/"),
        "不得给最终 key 签发写权限：{url}"
    );
    assert!(!url.to_lowercase().contains("secret"), "{url}");

    http_client()
        .put(url)
        .body(bytes.to_vec())
        .send()
        .await
        .expect("直传 PUT")
        .error_for_status()
        .expect("PUT 应成功");

    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = viewer_get(&h, &list_path).await;
    assert_eq!(resp.status(), 200);
    let body = common::body_json(resp).await;
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(0),
        "完成前产物不可见"
    );
    let resp = viewer_get(&h, &format!("{list_path}/dist.bin")).await;
    assert_eq!(resp.status(), 404, "完成前不可下载");

    let complete_path = format!("/api/v1/agent/artifacts/{}/dist.bin/complete", h.job_a);
    let resp = agent_post_json(
        &h,
        &complete_path,
        &format!(r#"{{"size":{},"sha256":"{digest}"}}"#, bytes.len()),
    )
    .await;
    assert_eq!(resp.status(), 201, "complete 应 201");
    let body = common::body_json(resp).await;
    assert_eq!(body["name"], "dist.bin");
    assert_eq!(body["sha256"], digest);

    let keys: Vec<String> = h.mock.objects.lock().unwrap().keys().cloned().collect();
    assert!(
        keys.iter().any(|k| k.contains("/artifacts/final/")),
        "应有最终对象：{keys:?}"
    );
    assert!(
        keys.iter().all(|k| !k.contains("/artifacts/tmp/")),
        "临时对象应已清理：{keys:?}"
    );
    let on_disk = h
        ._dir
        .path()
        .join("artifacts")
        .join(h.build.id.to_string())
        .join("dist.bin");
    assert!(!on_disk.exists(), "S3 产物不得落本地盘");

    let resp = viewer_get(&h, &list_path).await;
    let body = common::body_json(resp).await;
    assert_eq!(body["items"][0]["backend"], "s3");
    assert_eq!(body["items"][0]["state"], "ready");
    assert_eq!(body["items"][0]["sha256"], digest);

    let resp = viewer_get(&h, &format!("{list_path}/dist.bin")).await;
    assert_eq!(resp.status(), 302, "viewer 应获短期下载 URL");
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("Location");
    assert!(
        loc.contains("/artifacts/final/"),
        "下载 URL 指向最终 key：{loc}"
    );
    let got = http_client()
        .get(loc)
        .send()
        .await
        .expect("GET 最终对象")
        .bytes()
        .await
        .expect("读字节");
    assert_eq!(&got[..], bytes);

    let resp2 = viewer_get(&h, &format!("{list_path}/dist.bin")).await;
    assert_eq!(resp2.status(), 302, "过期 URL 可重新签发");
    let loc2 = resp2
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("可重新签发");
    assert!(
        loc2.contains("/artifacts/final/"),
        "重签仍指向最终 key：{loc2}"
    );

    let resp = agent_post_json(
        &h,
        &complete_path,
        &format!(r#"{{"size":{},"sha256":"{digest}"}}"#, bytes.len()),
    )
    .await;
    assert_eq!(resp.status(), 200, "ready 后 complete 幂等");

    let resp = agent_post_json(&h, &grant_path, "{}").await;
    assert_eq!(resp.status(), 409, "ready 后不得再签发写 URL");
    let resp = agent_post_json(
        &h,
        &complete_path,
        &format!(
            r#"{{"size":{},"sha256":"{}"}}"#,
            bytes.len(),
            "a".repeat(64)
        ),
    )
    .await;
    assert_eq!(resp.status(), 409, "ready 后不同摘要不得覆盖");

    let resp = agent_download(&h, h.job_b, "build", "dist.bin").await;
    assert_eq!(resp.status(), 302, "依赖拉取同样签发最终对象 GET");
}

async fn agent_download(
    h: &Harness,
    job_id: i64,
    source_job: &str,
    name: &str,
) -> axum::response::Response {
    custom_req(
        &h.app,
        "GET",
        &format!("/api/v1/agent/artifacts/{job_id}/downloads/{source_job}/{name}"),
        None,
        None,
        &[("authorization", format!("Bearer {}", h.agent_token))],
        DEFAULT_PEER,
    )
    .await
}

/// 声明的 SHA-256 与临时对象不符时不发布，并清理临时对象。
#[tokio::test]
async fn complete_rejects_hash_mismatch_and_cleans_tmp() {
    let h = harness().await;
    let bytes = b"s3-artifact-payload";
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        "{}",
    )
    .await;
    let url = common::body_json(grant).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    http_client()
        .put(&url)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let resp = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/complete", h.job_a),
        &format!(
            r#"{{"size":{},"sha256":"{}"}}"#,
            bytes.len(),
            "0".repeat(64)
        ),
    )
    .await;
    assert_eq!(resp.status(), 422);
    let keys: Vec<String> = h.mock.objects.lock().unwrap().keys().cloned().collect();
    assert!(
        keys.iter()
            .all(|k| !k.contains("/artifacts/tmp/") && !k.contains("/artifacts/final/")),
        "完整性失败应清理临时/最终对象：{keys:?}"
    );
    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = viewer_get(&h, &list_path).await;
    let body = common::body_json(resp).await;
    assert_eq!(
        body["items"].as_array().map(Vec::len),
        Some(0),
        "失败不得发布"
    );
}

/// 未在任务上传声明内的产物名不得签发 URL。
#[tokio::test]
async fn undeclared_upload_is_rejected() {
    let h = harness().await;
    let resp = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/secret.bin/upload-url", h.job_a),
        "{}",
    )
    .await;
    assert_eq!(resp.status(), 422);
    let body = common::body_json(resp).await;
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("声明"), "{msg}");
}

/// 未认证不能列举或下载；无项目角色与不存在同形 404。
#[tokio::test]
async fn unauthorized_cannot_list_or_download() {
    let h = harness().await;
    let bytes = b"s3-artifact-payload";
    let digest = sha256_hex(bytes);
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        "{}",
    )
    .await;
    let url = common::body_json(grant).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    http_client()
        .put(&url)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let complete = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/complete", h.job_a),
        &format!(r#"{{"size":{},"sha256":"{digest}"}}"#, bytes.len()),
    )
    .await;
    assert_eq!(complete.status(), 201);

    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = common::get(&h.app, &list_path).await;
    assert_eq!(resp.status(), 401);
    let resp = common::get(&h.app, &format!("{list_path}/dist.bin")).await;
    assert_eq!(resp.status(), 401);

    let phc = sisyphus_server::auth::hash_password_blocking("user-password-1");
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES ('dave', ?, 0, 0, 1, 1)",
    )
    .bind(&phc)
    .execute(&h.app.pool)
    .await
    .expect("直插无角色用户");
    let resp = common::post(
        &h.app,
        "/api/v1/auth/login",
        r#"{ "username": "dave", "password": "user-password-1" }"#,
    )
    .await;
    let cookie = common::cookie_of(&resp).expect("会话");
    let resp = common::req_with_cookie(&h.app, "GET", &list_path, None, Some(&cookie)).await;
    assert_eq!(resp.status(), 404, "无角色列举与不存在同形");
    let resp = common::req_with_cookie(
        &h.app,
        "GET",
        &format!("{list_path}/dist.bin"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), 404, "无角色下载与不存在同形");
}

/// 配置 S3 后旧本地产物仍可从构建详情读取。
#[tokio::test]
async fn local_artifact_still_readable_when_s3_configured() {
    let h = harness().await;
    let bytes = b"legacy-local";
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, name, path, size, sha256, created_at, retention_until, backend, state)
         VALUES (?, 'legacy.bin', ?, ?, ?, 0, ?, 'local', 'ready')",
    )
    .bind(h.build.id)
    .bind(format!("{}/legacy.bin", h.build.id))
    .bind(bytes.len() as i64)
    .bind(sha256_hex(bytes))
    .bind(30_i64 * 24 * 60 * 60 * 1000)
    .execute(&h.app.pool)
    .await
    .expect("插本地产物");
    let dir = h._dir.path().join("artifacts").join(h.build.id.to_string());
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("legacy.bin"), bytes)
        .await
        .unwrap();

    let list_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
        h.build.number
    );
    let resp = viewer_get(&h, &list_path).await;
    let body = common::body_json(resp).await;
    assert_eq!(body["items"][0]["backend"], "local");
    assert_eq!(body["items"][0]["state"], "ready");

    let resp = viewer_get(&h, &format!("{list_path}/legacy.bin")).await;
    assert_eq!(resp.status(), 200);
    let got = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&got[..], bytes);
}

/// S3 产物不按本地 30 天保留期清理。
#[tokio::test]
async fn s3_artifact_survives_local_retention_sweep() {
    let h = harness().await;
    let bytes = b"keep-me";
    let digest = sha256_hex(bytes);
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        "{}",
    )
    .await;
    let url = common::body_json(grant).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    http_client()
        .put(&url)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/complete", h.job_a),
        &format!(r#"{{"size":{},"sha256":"{digest}"}}"#, bytes.len()),
    )
    .await;

    let until: i64 = sqlx::query_scalar(
        "SELECT retention_until FROM artifacts WHERE build_id = ? AND name = 'dist.bin'",
    )
    .bind(h.build.id)
    .fetch_one(&h.app.pool)
    .await
    .expect("retention");
    assert_eq!(until, i64::MAX, "S3 默认永久保留");

    let report = sisyphus_server::store::sweep(
        &h.app.pool,
        &h._dir.path().join("artifacts"),
        crate_now_far_future(),
        30,
    )
    .await
    .expect("sweep");
    let _ = report;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifacts WHERE build_id = ? AND name = 'dist.bin' AND backend = 's3'",
    )
    .bind(h.build.id)
    .fetch_one(&h.app.pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "S3 行不被本地清理裁掉");
}

fn crate_now_far_future() -> i64 {
    i64::MAX / 2
}

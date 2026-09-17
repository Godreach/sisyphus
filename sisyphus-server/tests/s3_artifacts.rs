//! 票 #123：单文件产物经临时对象安全发布到 S3。
//!
//! 观测缝：Agent 预签名 PUT（临时 key）→ complete 校验复制 → 构建详情
//! viewer 获短期 GET URL；pending 完成前不可见。Mock S3 进程内 axum。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    uploads: Arc<Mutex<HashMap<String, (String, HashMap<u32, Vec<u8>>)>>>,
    next_upload: Arc<AtomicUsize>,
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

    if query == "uploads=" && method == axum::http::Method::POST {
        let id = format!(
            "upload-{}",
            mock.next_upload.fetch_add(1, Ordering::Relaxed)
        );
        mock.uploads
            .lock()
            .unwrap()
            .insert(id.clone(), (key.to_string(), HashMap::new()));
        return (
            StatusCode::OK,
            format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"),
        )
            .into_response();
    }
    let upload_id = query_val(&query, "uploadId");
    let part_number = query_val(&query, "partNumber").and_then(|v| v.parse::<u32>().ok());
    if let Some(upload_id) = upload_id {
        if method == axum::http::Method::DELETE {
            let removed = mock.uploads.lock().unwrap().remove(&upload_id);
            return if removed.is_some() {
                StatusCode::NO_CONTENT.into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    "<Error><Code>NoSuchUpload</Code></Error>",
                )
                    .into_response()
            };
        }
        if method == axum::http::Method::PUT {
            let Some(part_number) = part_number else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let mut uploads = mock.uploads.lock().unwrap();
            let Some((_key, parts)) = uploads.get_mut(&upload_id) else {
                return (
                    StatusCode::NOT_FOUND,
                    "<Error><Code>NoSuchUpload</Code></Error>",
                )
                    .into_response();
            };
            let data = if let Some(src) = headers
                .get("x-amz-copy-source")
                .and_then(|v| v.to_str().ok())
            {
                let src_key = copy_source_key(src, &mock.bucket);
                let source = mock
                    .objects
                    .lock()
                    .unwrap()
                    .get(&src_key)
                    .cloned()
                    .unwrap_or_default();
                headers
                    .get("x-amz-copy-source-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_range)
                    .and_then(|(start, end)| source.get(start..=end.min(source.len() - 1)))
                    .unwrap_or(&source)
                    .to_vec()
            } else {
                body.to_vec()
            };
            parts.insert(part_number, data);
            return (
                StatusCode::OK,
                [("etag", format!("\"etag-{part_number}\""))],
                format!("<CopyPartResult><ETag>etag-{part_number}</ETag></CopyPartResult>"),
            )
                .into_response();
        }
        if method == axum::http::Method::POST {
            let Some((object_key, parts)) = mock.uploads.lock().unwrap().remove(&upload_id) else {
                return (
                    StatusCode::NOT_FOUND,
                    "<Error><Code>NoSuchUpload</Code></Error>",
                )
                    .into_response();
            };
            let mut ordered: Vec<_> = parts.into_iter().collect();
            ordered.sort_by_key(|(number, _)| *number);
            let data = ordered
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect::<Vec<_>>();
            mock.objects.lock().unwrap().insert(object_key, data);
            return StatusCode::OK.into_response();
        }
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

fn query_val(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        (parts.next()? == name).then(|| parts.next().unwrap_or_default().to_string())
    })
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

async fn spawn_mock(access_key: &str, bucket: &str) -> (SocketAddr, Mock) {
    let mock = Mock {
        access_key: access_key.into(),
        bucket: bucket.into(),
        objects: Arc::new(Mutex::new(HashMap::new())),
        uploads: Arc::new(Mutex::new(HashMap::new())),
        next_upload: Arc::new(AtomicUsize::new(1)),
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
    harness_with_limits(sisyphus_server::api::ArtifactTransferLimits::default()).await
}

async fn harness_with_limits(limits: sisyphus_server::api::ArtifactTransferLimits) -> Harness {
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
    .with_artifact_transfer_limits(limits)
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
        artifact_downloads: if name == "package" {
            vec![sisyphus_model::pipeline::ArtifactDownload {
                job: "build".into(),
                name: "dist.bin".into(),
                path: "dist".into(),
            }]
        } else {
            vec![]
        },
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
                    job_def(
                        "build",
                        vec![
                            ("dist.bin", "dist.bin"),
                            ("dist file.bin", "dist file.bin"),
                            ("other.bin", "other.bin"),
                        ],
                    ),
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
    let spec_a = r#"{"artifact_uploads":[{"name":"dist.bin","path":"dist.bin"},{"name":"dist file.bin","path":"dist file.bin"},{"name":"other.bin","path":"other.bin"}]}"#;
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
            spec_json: Some(r#"{"artifact_uploads":[],"artifact_downloads":[{"job":"build","name":"dist.bin","path":"dist"}]}"#.into()),
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

#[tokio::test]
async fn directory_set_binds_uploads_and_publishes_only_complete_manifest() {
    let h = harness().await;
    let bytes = b"abc";
    let digest = sha256_hex(bytes);
    let create = format!("/api/v1/agent/artifacts/{}/sets", h.job_a);
    let payload = serde_json::json!({"name":"dist.bin","entries":[
        {"path":"empty","kind":"directory","size":0,"sha256":"","executable":false},
        {"path":"nested/a.txt","kind":"file","size":3,"sha256":digest,"executable":true}
    ]})
    .to_string();
    let response = agent_post_json(&h, &create, &payload).await;
    assert_eq!(response.status(), 200);
    let manifest = common::body_json(response).await;
    let set_id = manifest["set"]["id"].as_i64().unwrap();
    let internal = manifest["entries"][1]["artifact_name"].as_str().unwrap();
    let publish = format!("{create}/{set_id}/publish");
    assert_eq!(agent_post_json(&h, &publish, "{}").await.status(), 409);
    let list = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifact-sets",
        h.build.number
    );
    assert_eq!(
        common::body_json(viewer_get(&h, &list).await).await["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let retry = common::body_json(agent_post_json(&h, &create, &payload).await).await;
    assert_eq!(retry["set"]["id"], set_id);
    let grant = format!("/api/v1/agent/artifacts/{}/{internal}/upload-url", h.job_a);
    let complete = format!("/api/v1/agent/artifacts/{}/{internal}/complete", h.job_a);
    for endpoint in ["upload-url", "complete"] {
        let spoof = format!("/api/v1/agent/artifacts/{}/{internal}/{endpoint}", h.job_b);
        assert_eq!(
            agent_post_json(&h, &spoof, &format!(r#"{{"size":3,"sha256":"{digest}"}}"#))
                .await
                .status(),
            404
        );
        let invented = format!(
            "/api/v1/agent/artifacts/{}/.set-999999-0/{endpoint}",
            h.job_a
        );
        assert_eq!(
            agent_post_json(
                &h,
                &invented,
                &format!(r#"{{"size":3,"sha256":"{digest}"}}"#)
            )
            .await
            .status(),
            404
        );
    }
    assert_eq!(
        agent_post_json(&h, &grant, r#"{"size":4}"#).await.status(),
        409
    );
    assert_eq!(
        agent_post_json(
            &h,
            &complete,
            &format!(r#"{{"size":4,"sha256":"{digest}"}}"#)
        )
        .await
        .status(),
        409
    );
    let upload = common::body_json(agent_post_json(&h, &grant, r#"{"size":3}"#).await).await;
    http_client()
        .put(upload["url"].as_str().unwrap())
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let completion = format!(r#"{{"size":3,"sha256":"{digest}"}}"#);
    assert_eq!(
        agent_post_json(&h, &complete, &completion).await.status(),
        201
    );
    assert_eq!(agent_post_json(&h, &publish, "{}").await.status(), 201);
    assert_eq!(agent_post_json(&h, &publish, "{}").await.status(), 200);
    let listed = common::body_json(viewer_get(&h, &list).await).await;
    assert_eq!(listed["items"][0]["availability"], "ready");
    assert_eq!(listed["items"][0]["entries"][1]["sha256"], digest);
    assert_eq!(listed["items"][0]["set"]["job_id"], h.job_a);
    assert_eq!(
        agent_post_json(&h, &grant, r#"{"size":3}"#).await.status(),
        404,
        "发布后的集合不可再写入"
    );
    let file = format!("{list}/{set_id}/file?path=nested%2Fa.txt");
    assert_eq!(viewer_get(&h, &file).await.status(), 302);
    let agent_file = format!(
        "/api/v1/agent/artifacts/{}/sets/{set_id}/file?path=nested%2Fa.txt",
        h.job_b
    );
    assert_eq!(
        custom_req(
            &h.app,
            "GET",
            &agent_file,
            None,
            None,
            &[("authorization", format!("Bearer {}", h.agent_token))],
            DEFAULT_PEER
        )
        .await
        .status(),
        302
    );
    let unauthorized = format!(
        "/api/v1/agent/artifacts/{}/sets/{set_id}/file?path=nested%2Fa.txt",
        h.job_a
    );
    assert_eq!(
        custom_req(
            &h.app,
            "GET",
            &unauthorized,
            None,
            None,
            &[("authorization", format!("Bearer {}", h.agent_token))],
            DEFAULT_PEER
        )
        .await
        .status(),
        404
    );
    let pending_body = common::body_json(
        viewer_get(
            &h,
            &format!(
                "/api/v1/projects/demo/pipelines/release/builds/{}/artifacts",
                h.build.number
            ),
        )
        .await,
    )
    .await;
    assert!(
        pending_body["items"].as_array().unwrap().is_empty(),
        "内部文件不能独立列出"
    );
    h.mock.objects.lock().unwrap().clear();
    let missing = common::body_json(viewer_get(&h, &list).await).await;
    assert_eq!(missing["items"][0]["entries"][1]["state"], "missing");
}

#[tokio::test]
async fn directory_manifest_rejects_portable_path_conflicts_and_supports_empty_root() {
    let h = harness().await;
    let create = format!("/api/v1/agent/artifacts/{}/sets", h.job_a);
    let entry = |path: &str, kind: &str| {
        serde_json::json!({"path":path,"kind":kind,"size":0,
        "sha256": if kind == "file" { sha256_hex(b"") } else { String::new() }, "executable":false})
    };
    for entries in [
        vec![entry("../escape", "file")],
        vec![entry("CON.txt", "file")],
        vec![entry("trailing.", "file")],
        vec![entry("A.txt", "file"), entry("a.txt", "file")],
        vec![entry("parent", "file"), entry("parent/child", "file")],
    ] {
        let body = serde_json::json!({"name":"dist.bin", "entries":entries}).to_string();
        assert_eq!(agent_post_json(&h, &create, &body).await.status(), 422);
    }
    let empty = r#"{"name":"dist.bin","entries":[]}"#;
    let manifest = common::body_json(agent_post_json(&h, &create, empty).await).await;
    let changed =
        serde_json::json!({"name":"dist.bin", "entries":[entry("extra", "directory")]}).to_string();
    assert_eq!(
        agent_post_json(&h, &create, &changed).await.status(),
        422,
        "空清单重试也不可改变"
    );
    let id = manifest["set"]["id"].as_i64().unwrap();
    assert_eq!(
        agent_post_json(&h, &format!("{create}/{id}/publish"), "{}")
            .await
            .status(),
        201
    );
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

    let resp = agent_post_json(&h, &grant_path, &format!(r#"{{"size":{}}}"#, bytes.len())).await;
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

    // 旧预签名 URL 即使在有效期内被重放，也只会改写已脱离发布链的临时 key；
    // 最终 key 从未签发写权限，下载内容保持不变。
    let replay = http_client()
        .put(url)
        .body(b"replayed-evil-bytes".to_vec())
        .send()
        .await
        .unwrap();
    assert!(replay.status().is_success(), "mock 接受仍有效的旧 URL");
    let final_bytes = h
        .mock
        .objects
        .lock()
        .unwrap()
        .iter()
        .find(|(key, _)| key.contains("/artifacts/final/"))
        .map(|(_, value)| value.clone())
        .expect("最终对象");
    assert_eq!(final_bytes, bytes, "旧临时写 URL 不得改写 ready 对象");

    let resp = agent_post_json(&h, &grant_path, &format!(r#"{{"size":{}}}"#, bytes.len())).await;
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

/// #124：超过阈值后 multipart 上传，且超过单次复制边界后 multipart copy；
/// ready 后旧分片 URL 已失效，不能改写最终对象。
#[tokio::test]
async fn large_file_uses_multipart_upload_and_copy_and_rejects_replay() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 100,
        task_limit: 200,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 6,
        copy_part_size: 4,
    })
    .await;
    let bytes = b"abcdefghij";
    let digest = sha256_hex(bytes);
    let grant = agent_post_json(
        &h,
        &format!(
            "/api/v1/agent/artifacts/{}/dist%20file.bin/upload-url",
            h.job_a
        ),
        &format!(r#"{{"size":{}}}"#, bytes.len()),
    )
    .await;
    assert_eq!(grant.status(), 200);
    let grant = common::body_json(grant).await;
    assert_eq!(grant["mode"], "multipart");
    assert_eq!(grant["part_size"], 4);
    let upload_id = grant["upload_id"].as_str().unwrap().to_string();
    let parts = grant["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 3);
    let old_url = parts[0]["url"].as_str().unwrap().to_string();
    let mut completed = Vec::new();
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let response = http_client()
            .put(parts[index]["url"].as_str().unwrap())
            .body(chunk.to_vec())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        completed.push(serde_json::json!({
            "part_number": index + 1,
            "etag": etag,
        }));
    }
    let complete = agent_post_json(
        &h,
        &format!(
            "/api/v1/agent/artifacts/{}/dist%20file.bin/complete",
            h.job_a
        ),
        &serde_json::json!({
            "size": bytes.len(),
            "sha256": digest,
            "upload_id": upload_id,
            "parts": completed,
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        complete.status(),
        201,
        "{}",
        common::body_text(complete).await
    );

    let replay = http_client()
        .put(old_url)
        .body(b"evil".to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        !replay.status().is_success(),
        "已完成 upload 的旧 URL 应失效"
    );
    let final_bytes = h
        .mock
        .objects
        .lock()
        .unwrap()
        .iter()
        .find(|(key, _)| key.contains("/artifacts/final/"))
        .map(|(_, value)| value.clone())
        .expect("最终对象");
    assert_eq!(final_bytes, bytes);
}

/// 过期 pending 会话在再次申请前被 abort；旧分片 URL 随即失效，新会话可重试。
#[tokio::test]
async fn expired_multipart_upload_is_aborted_before_retry() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 100,
        task_limit: 200,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 100,
        copy_part_size: 4,
    })
    .await;
    let grant_path = format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a);
    let first = agent_post_json(&h, &grant_path, r#"{"size":10}"#).await;
    assert_eq!(first.status(), 200);
    let first = common::body_json(first).await;
    let first_id = first["upload_id"].as_str().unwrap().to_string();
    let old_url = first["parts"][0]["url"].as_str().unwrap().to_string();
    sqlx::query("UPDATE artifact_multipart_uploads SET expires_at = 0")
        .execute(&h.app.pool)
        .await
        .expect("模拟过期");

    let second = agent_post_json(&h, &grant_path, r#"{"size":10}"#).await;
    assert_eq!(second.status(), 200);
    let second = common::body_json(second).await;
    assert_ne!(second["upload_id"].as_str().unwrap(), first_id);
    let replay = http_client()
        .put(old_url)
        .body(b"old".to_vec())
        .send()
        .await
        .unwrap();
    assert!(!replay.status().is_success(), "过期会话必须已 abort");
    assert_eq!(h.mock.uploads.lock().unwrap().len(), 1, "只保留新会话");
}

/// 单文件与单任务限额都在签发任何 S3 写 URL 前拒绝。
#[tokio::test]
async fn transfer_limits_reject_before_upload_grant() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 10,
        task_limit: 12,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 6,
        copy_part_size: 4,
    })
    .await;
    let path = format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a);
    let too_large = agent_post_json(&h, &path, r#"{"size":11}"#).await;
    assert_eq!(too_large.status(), 422);
    assert!(h.mock.uploads.lock().unwrap().is_empty());

    sqlx::query(
        "INSERT INTO artifacts
            (build_id, name, path, size, sha256, created_at, retention_until,
             backend, job_id, attempt, state)
         VALUES (?, 'other.bin', 'other', 8, '', 0, ?, 's3', ?, 1, 'ready')",
    )
    .bind(h.build.id)
    .bind(i64::MAX)
    .bind(h.job_a)
    .execute(&h.app.pool)
    .await
    .expect("已有任务产物");
    let task_too_large = agent_post_json(&h, &path, r#"{"size":5}"#).await;
    assert_eq!(task_too_large.status(), 422);
    let body = common::body_json(task_too_large).await;
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("单任务"),
        "{body}"
    );
    assert!(h.mock.uploads.lock().unwrap().is_empty());
}

/// 完整清单的合计限额在第一个写 URL 签发之前拒绝。
#[tokio::test]
async fn preflight_rejects_task_total_before_any_transfer() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 10,
        task_limit: 12,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 6,
        copy_part_size: 4,
    })
    .await;
    let resp = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/preflight", h.job_a),
        r#"{"files":[{"name":"dist.bin","size":8},{"name":"other.bin","size":5}]}"#,
    )
    .await;
    assert_eq!(resp.status(), 422);
    let body = common::body_json(resp).await;
    assert!(body["message"].as_str().unwrap().contains("单任务"));
    assert!(h.mock.objects.lock().unwrap().is_empty());
    assert!(h.mock.uploads.lock().unwrap().is_empty());
}

/// 不能先申请较小的写许可，再在 complete 声明超限大小绕过限额。
#[tokio::test]
async fn complete_rejects_size_changed_after_grant() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 10,
        task_limit: 12,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 6,
        copy_part_size: 4,
    })
    .await;
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        r#"{"size":4}"#,
    )
    .await;
    assert_eq!(grant.status(), 200);
    let url = common::body_json(grant).await["url"]
        .as_str()
        .unwrap()
        .to_string();
    http_client()
        .put(url)
        .body(b"12345678901".to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let complete = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/complete", h.job_a),
        &serde_json::json!({
            "size": 11,
            "sha256": sha256_hex(b"12345678901"),
        })
        .to_string(),
    )
    .await;
    assert_eq!(complete.status(), 422);
    assert!(
        h.mock
            .objects
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("/artifacts/final/"))
    );
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
        &format!(r#"{{"size":{}}}"#, bytes.len()),
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
        r#"{"size":1}"#,
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
        &format!(r#"{{"size":{}}}"#, bytes.len()),
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
        &format!(r#"{{"size":{}}}"#, bytes.len()),
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

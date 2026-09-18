//! 票 #123：单文件产物经临时对象安全发布到 S3。
//!
//! 观测缝：Agent 预签名 PUT（临时 key）→ complete 校验复制 → 构建详情
//! viewer 获短期 GET URL；pending 完成前不可见。Mock S3 进程内 axum。

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use common::{DEFAULT_PEER, custom_req};
use futures::StreamExt;
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
use std::io::Write;
use tower::ServiceExt;

#[derive(Clone)]
struct Mock {
    access_key: String,
    bucket: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    uploads: Arc<Mutex<HashMap<String, (String, HashMap<u32, Vec<u8>>)>>>,
    next_upload: Arc<AtomicUsize>,
    fail_deletes: Arc<AtomicBool>,
    ranges: Arc<Mutex<Vec<String>>>,
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
                mock.ranges.lock().unwrap().push(range.to_string());
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
            if mock.fail_deletes.load(Ordering::Relaxed) && key.contains("/artifacts/final/") {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "<Error><Code>ServiceUnavailable</Code><Message>retry</Message></Error>",
                )
                    .into_response();
            }
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
        fail_deletes: Arc::new(AtomicBool::new(false)),
        ranges: Arc::new(Mutex::new(Vec::new())),
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
    harness_with_limits_and_backend(
        sisyphus_server::api::ArtifactTransferLimits::default(),
        None,
    )
    .await
}

async fn harness_with_limits(limits: sisyphus_server::api::ArtifactTransferLimits) -> Harness {
    harness_with_limits_and_backend(limits, None).await
}

async fn harness_with_limits_and_backend(
    limits: sisyphus_server::api::ArtifactTransferLimits,
    backend: Option<sisyphus_server::config::LogArchiveBackend>,
) -> Harness {
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
    let mut state = api::AppState::new(
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
    if let Some(backend) = backend {
        state = state.with_log_archive_backend(backend);
    }
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

fn two_frame_log_archive(job_id: i64) -> (Vec<u8>, serde_json::Value) {
    let mut archive = b"SYLOGA01".to_vec();
    let mut frames = Vec::new();
    let mut raw_bytes = 0;
    for (seq, data) in [(0, "b25lCg"), (1, "dHdvCg")] {
        let line =
            format!("{{\"seq\":{seq},\"kind\":\"output\",\"stream\":0,\"data\":\"{data}\"}}\n");
        raw_bytes += line.len();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(line.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let offset = archive.len() as u64 + 8;
        archive.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
        archive.extend_from_slice(&compressed);
        frames.push(serde_json::json!({
            "start_seq": seq, "end_seq": seq, "offset": offset,
            "compressed_len": compressed.len()
        }));
    }
    let index = serde_json::json!({
        "job_id": job_id.to_string(), "attempt": 1,
        "first_seq": 0, "last_seq": 1, "raw_bytes": raw_bytes,
        "compressed_bytes": archive.len(), "sha256": sha256_hex(&archive),
        "frames": frames
    });
    (archive, index)
}

#[tokio::test]
async fn log_archive_grant_only_signs_a_temporary_log_object() {
    let h = harness().await;
    let bytes = b"SYLOGA01";
    let request = serde_json::json!({
        "index": {
            "job_id": h.job_a.to_string(), "attempt": 1,
            "first_seq": null, "last_seq": null, "raw_bytes": 0,
            "compressed_bytes": bytes.len(), "sha256": sha256_hex(bytes),
            "frames": []
        }
    });
    let path = format!("/api/v1/agent/log-archives/{}/1/upload-url", h.job_a);
    let response = agent_post_json(&h, &path, &request.to_string()).await;
    assert_eq!(response.status(), 200);
    let grant = common::body_json(response).await;
    assert_eq!(grant["backend"], "s3");
    let url = grant["url"].as_str().expect("预签名 URL");
    assert!(url.contains("/logs/tmp/"), "只能签临时日志 key：{url}");
    assert!(!url.contains("/logs/final/"));
}

#[tokio::test]
async fn configured_local_log_backend_keeps_server_upload_when_s3_exists() {
    let h = harness_with_limits_and_backend(
        sisyphus_server::api::ArtifactTransferLimits::default(),
        Some(sisyphus_server::config::LogArchiveBackend::Local),
    )
    .await;
    let (archive, index) = two_frame_log_archive(h.job_a);
    let path = format!("/api/v1/agent/log-archives/{}/1/upload-url", h.job_a);
    let response =
        agent_post_json(&h, &path, &serde_json::json!({"index": index}).to_string()).await;
    assert_eq!(response.status(), 200);
    let grant = common::body_json(response).await;
    assert_eq!(grant["backend"], "local");
    assert!(grant.get("url").is_none());
    assert!(
        h.mock
            .objects
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("/logs/"))
    );

    let upload = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/agent/log-archives/{}/1?size={}&sha256={}",
            h.job_a,
            archive.len(),
            sha256_hex(&archive)
        ))
        .header("authorization", format!("Bearer {}", h.agent_token))
        .header("x-sisyphus-archive-index", index.to_string())
        .extension(axum::extract::ConnectInfo(DEFAULT_PEER))
        .body(axum::body::Body::from(archive))
        .unwrap();
    let response = h.app.router.clone().oneshot(upload).await.unwrap();
    assert_eq!(response.status(), 200);

    // 后续部署切回 S3 时，既有 Server 本地归档仍从原路径读取。
    let state = h
        .app
        .state
        .clone()
        .with_log_archive_backend(sisyphus_server::config::LogArchiveBackend::S3);
    let app = common::test_app_from_state(state, h._dir.path());
    let response = custom_req(
        &app,
        "GET",
        &format!(
            "/api/v1/projects/demo/pipelines/release/builds/{}/jobs/build/attempts/1/logs",
            h.build.number
        ),
        None,
        Some(&h.cookie),
        &[],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(common::body_text(response).await, "one\ntwo\n");
}

#[tokio::test]
async fn s3_log_archive_is_immutable_and_downloaded_through_server() {
    let h = harness().await;
    let (archive, index) = two_frame_log_archive(h.job_a);
    let grant_path = format!("/api/v1/agent/log-archives/{}/1/upload-url", h.job_a);
    let response = agent_post_json(
        &h,
        &grant_path,
        &serde_json::json!({"index": index}).to_string(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let grant = common::body_json(response).await;
    let url = grant["url"].as_str().unwrap();
    http_client()
        .put(url)
        .body(archive.clone())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let complete = format!("/api/v1/agent/log-archives/{}/1/complete", h.job_a);
    let response = agent_post_json(&h, &complete, "{}").await;
    assert_eq!(response.status(), 200);
    assert_eq!(common::body_json(response).await["state"], "ready");
    let key = h
        .mock
        .objects
        .lock()
        .unwrap()
        .keys()
        .find(|key| key.contains("/logs/final/"))
        .cloned()
        .expect("最终 key");
    assert!(
        h.mock
            .objects
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("/logs/tmp/"))
    );

    // 旧上传许可只会改变临时 key；已确认的归档正文始终不变。
    http_client()
        .put(url)
        .body(b"tampered".to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(h.mock.objects.lock().unwrap().get(&key), Some(&archive));
    assert_eq!(agent_post_json(&h, &complete, "{}").await.status(), 200);

    let path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/jobs/build/attempts/1/logs",
        h.build.number
    );
    let before = h.mock.ranges.lock().unwrap().len();
    let response = viewer_get(&h, &path).await;
    assert_eq!(response.status(), 200);
    assert_eq!(common::body_text(response).await, "one\ntwo\n");
    assert_eq!(
        h.mock.ranges.lock().unwrap().len() - before,
        2,
        "下载逐帧 Range GET"
    );

    let response = viewer_get(&h, &format!("{path}/stream?from=1")).await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("SSE 回放及时返回")
        .expect("有 seq=1 事件")
        .unwrap();
    let text = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(
        text.contains("id: 1") && text.contains("two"),
        "按 seq 回放：{text}"
    );
    assert!(!text.contains("id: 0"), "不回放游标之前事件：{text}");
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

/// #128：后台删除失败可见，显式重试后幂等完成；成功前保留正文元数据，
/// 成功后同时移除 S3 对象和正文元数据。
#[tokio::test]
async fn queued_build_deletion_reports_failure_and_retries_idempotently() {
    let h = harness().await;
    let object_key = "prod/artifacts/final/manual-delete";
    h.mock
        .objects
        .lock()
        .unwrap()
        .insert(object_key.into(), b"bytes".to_vec());
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, job_id, attempt, backend, state, name, path, size, sha256,
             created_at, retention_until)
         VALUES (?, ?, 1, 's3', 'ready', 'dist.bin', ?, 5, 'abc', 1, ?)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .bind(object_key)
    .bind(i64::MAX)
    .execute(&h.app.pool)
    .await
    .expect("插入 S3 元数据");
    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Succeeded,
                3
            )
            .await
            .expect("终态")
    );

    h.mock.fail_deletes.store(true, Ordering::Relaxed);
    let path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}?delete_s3_artifacts=true",
        h.build.number
    );
    let resp = common::req_with_cookie(&h.app, "DELETE", &path, None, Some(&h.cookie)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let job = common::body_json(resp).await;
    let job_id = job["id"].as_i64().expect("删除任务 id");

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("执行失败轮次")
    );
    let status_path = "/api/v1/projects/demo/artifact-deletions";
    let status = common::body_json(viewer_get(&h, status_path).await).await;
    assert_eq!(status["items"][0]["state"], "failed");
    assert_eq!(status["items"][0]["attempts"], 1);
    assert!(status["items"][0]["last_error"].as_str().is_some());
    assert!(h.mock.objects.lock().unwrap().contains_key(object_key));
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE build_id = ?")
        .bind(h.build.id)
        .fetch_one(&h.app.pool)
        .await
        .expect("元数据数");
    assert_eq!(remaining, 1, "失败时保留元数据供重试");

    h.mock.fail_deletes.store(false, Ordering::Relaxed);
    let retry_path = format!("/api/v1/projects/demo/artifact-deletions/{job_id}/retry");
    let resp = common::req_with_cookie(
        &h.app,
        "POST",
        &retry_path,
        Some("{}".into()),
        Some(&h.cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(common::body_json(resp).await["state"], "queued");

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("重试")
    );
    let status = common::body_json(viewer_get(&h, status_path).await).await;
    assert_eq!(status["items"][0]["state"], "completed");
    assert_eq!(status["items"][0]["attempts"], 2);
    assert!(!h.mock.objects.lock().unwrap().contains_key(object_key));
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE build_id = ?")
        .bind(h.build.id)
        .fetch_one(&h.app.pool)
        .await
        .expect("元数据数");
    assert_eq!(remaining, 0);
    assert!(
        !sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("完成后空转"),
        "完成任务不得重复执行"
    );
}

/// #128：构建清理必须中止仍在传输的 multipart 会话，并裁剪临时会话与
/// pending 元数据，不能只删除已经发布的最终对象。
#[tokio::test]
async fn build_deletion_aborts_pending_multipart_uploads() {
    let h = harness_with_limits(sisyphus_server::api::ArtifactTransferLimits {
        single_file_limit: 100,
        task_limit: 200,
        multipart_threshold: 5,
        multipart_part_size: 4,
        copy_object_limit: 6,
        copy_part_size: 4,
    })
    .await;
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        r#"{"size":10}"#,
    )
    .await;
    assert_eq!(grant.status(), StatusCode::OK);
    assert_eq!(common::body_json(grant).await["mode"], "multipart");
    assert_eq!(h.mock.uploads.lock().unwrap().len(), 1);

    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Failed,
                3
            )
            .await
            .expect("终态")
    );
    let path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}?delete_s3_artifacts=true",
        h.build.number
    );
    let resp = common::req_with_cookie(&h.app, "DELETE", &path, None, Some(&h.cookie)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("清理 multipart")
    );
    assert!(
        h.mock.uploads.lock().unwrap().is_empty(),
        "应中止 S3 multipart"
    );
    let sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM artifact_multipart_uploads WHERE build_id = ?")
            .bind(h.build.id)
            .fetch_one(&h.app.pool)
            .await
            .expect("multipart 会话数");
    assert_eq!(sessions, 0);
    let metas: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE build_id = ?")
        .bind(h.build.id)
        .fetch_one(&h.app.pool)
        .await
        .expect("产物元数据数");
    assert_eq!(metas, 0);
}

/// #128：单 PUT 已写入但尚未 complete 的临时对象同样属于构建空间，清理
/// 不能只看 artifacts.path 指向的最终键。
#[tokio::test]
async fn build_deletion_removes_pending_single_upload_object() {
    let h = harness().await;
    let grant = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        r#"{"size":1}"#,
    )
    .await;
    let url = common::body_json(grant).await["url"]
        .as_str()
        .expect("单 PUT URL")
        .to_string();
    http_client()
        .put(url)
        .body(vec![b'x'])
        .send()
        .await
        .expect("写临时对象")
        .error_for_status()
        .expect("临时对象写入成功");
    assert!(
        h.mock
            .objects
            .lock()
            .unwrap()
            .keys()
            .any(|key| key.contains("/artifacts/tmp/"))
    );

    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Failed,
                3
            )
            .await
            .expect("终态")
    );
    let path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}?delete_s3_artifacts=true",
        h.build.number
    );
    let resp = common::req_with_cookie(&h.app, "DELETE", &path, None, Some(&h.cookie)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("清理单 PUT 临时对象")
    );
    assert!(
        h.mock
            .objects
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("/artifacts/tmp/")),
        "pending 单 PUT 临时对象应清理"
    );
}

/// #128：任务一旦被认领，枚举目标失败也必须持久化为 failed，不能永久
/// 卡在 running 而失去重试入口。
#[tokio::test]
async fn deletion_target_lookup_error_is_recorded_as_failed() {
    let h = harness().await;
    h.app
        .state
        .deletions
        .enqueue_build(1, h.build.id, "admin")
        .await
        .expect("删除入队");
    sqlx::query("DROP TABLE artifacts")
        .execute(&h.app.pool)
        .await
        .expect("制造枚举失败");

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("失败应被任务吸收")
    );
    let jobs = h
        .app
        .state
        .deletions
        .list_by_project(1)
        .await
        .expect("删除状态");
    assert_eq!(
        jobs[0].state,
        sisyphus_server::store::deletions::DeletionState::Failed
    );
    assert!(jobs[0].last_error.as_deref().is_some_and(|e| !e.is_empty()));
}

/// #128：S3 删除成功后若元数据事务失败，任务同样回到 failed，保留可见
/// 错误与重试入口。
#[tokio::test]
async fn deletion_completion_error_is_recorded_as_failed() {
    let h = harness().await;
    let project_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(&h.app.pool)
        .await
        .expect("项目 id");
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, job_id, attempt, backend, state, name, path, size, sha256,
             created_at, retention_until)
         VALUES (?, ?, 1, 's3', 'ready', 'dist.bin',
                 'prod/artifacts/final/complete-error', 1, 'abc', 1, ?)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .bind(i64::MAX)
    .execute(&h.app.pool)
    .await
    .expect("S3 元数据");
    h.app
        .state
        .deletions
        .enqueue_build(project_id, h.build.id, "admin")
        .await
        .expect("删除入队");
    sqlx::raw_sql(
        "CREATE TRIGGER reject_artifact_delete
         BEFORE DELETE ON artifacts
         BEGIN SELECT RAISE(ABORT, 'completion blocked'); END;",
    )
    .execute(&h.app.pool)
    .await
    .expect("制造完成事务失败");

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("失败应被任务吸收")
    );
    let jobs = h
        .app
        .state
        .deletions
        .list_by_project(project_id)
        .await
        .expect("删除状态");
    assert_eq!(
        jobs[0].state,
        sisyphus_server::store::deletions::DeletionState::Failed
    );
    assert!(
        jobs[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("completion blocked"))
    );
}

/// #128：只允许项目管理员删除整个产物集；运行中构建拒绝清理，受理后整集
/// 立即停止列举和签 URL，后台完成后对象与集合元数据一并移除。
#[tokio::test]
async fn artifact_set_delete_is_whole_set_only_and_rejects_live_build() {
    let h = harness().await;
    let set_id = sqlx::query(
        "INSERT INTO artifact_sets (build_id, job_id, attempt, name, state, created_at)
         VALUES (?, ?, 1, 'bundle', 'ready', 1)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .execute(&h.app.pool)
    .await
    .expect("产物集")
    .last_insert_rowid();
    let internal = format!(".set-{set_id}-0");
    sqlx::query(
        "INSERT INTO artifact_set_entries
            (set_id, path, kind, size, sha256, executable, artifact_name)
         VALUES (?, 'nested/a.txt', 'file', 3, 'abc', 0, ?)",
    )
    .bind(set_id)
    .bind(&internal)
    .execute(&h.app.pool)
    .await
    .expect("集合条目");
    let object_key = "prod/artifacts/final/set-delete";
    h.mock
        .objects
        .lock()
        .unwrap()
        .insert(object_key.into(), b"abc".to_vec());
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, job_id, attempt, backend, state, name, path, size, sha256,
             created_at, retention_until)
         VALUES (?, ?, 1, 's3', 'ready', ?, ?, 3, 'abc', 1, ?)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .bind(&internal)
    .bind(object_key)
    .bind(i64::MAX)
    .execute(&h.app.pool)
    .await
    .expect("集合正文");

    let base = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifact-sets/{set_id}",
        h.build.number
    );
    let resp = common::req_with_cookie(&h.app, "DELETE", &base, None, Some(&h.cookie)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "排队构建不可删产物集");

    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Succeeded,
                3
            )
            .await
            .expect("终态")
    );
    let resp = common::req_with_cookie(&h.app, "DELETE", &base, None, Some(&h.cookie)).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let job = common::body_json(resp).await;
    assert_eq!(job["scope"], "set");
    assert_eq!(job["set_id"], set_id);

    let list = format!(
        "/api/v1/projects/demo/pipelines/release/builds/{}/artifact-sets",
        h.build.number
    );
    let body = common::body_json(viewer_get(&h, &list).await).await;
    assert_eq!(body["items"], serde_json::json!([]), "deleting 整集隐藏");
    let file = format!("{base}/file?path=nested%2Fa.txt");
    assert_eq!(viewer_get(&h, &file).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        common::req_with_cookie(&h.app, "DELETE", &file, None, Some(&h.cookie))
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "不能单独删除集合内文件"
    );

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("执行")
    );
    assert!(!h.mock.objects.lock().unwrap().contains_key(object_key));
    let sets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifact_sets WHERE id = ?")
        .bind(set_id)
        .fetch_one(&h.app.pool)
        .await
        .expect("集合数");
    assert_eq!(sets, 0);
}

/// #128：项目删除由全局管理员发起，受理后立即冻结项目授权；后台清理 S3、
/// 旧本地产物与日志，清理期间和完成后均可在全局删除状态面追踪，并写审计。
#[tokio::test]
async fn project_delete_freezes_access_and_cleans_all_owned_data() {
    let h = harness().await;
    let project_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(&h.app.pool)
        .await
        .expect("项目 id");
    let object_key = "prod/artifacts/final/project-delete";
    h.mock
        .objects
        .lock()
        .unwrap()
        .insert(object_key.into(), b"remote".to_vec());
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, job_id, attempt, backend, state, name, path, size, sha256,
             created_at, retention_until)
         VALUES (?, ?, 1, 's3', 'ready', 'remote.bin', ?, 6, 'abc', 1, ?)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .bind(object_key)
    .bind(i64::MAX)
    .execute(&h.app.pool)
    .await
    .expect("S3 元数据");
    let local_dir = h._dir.path().join("artifacts").join(h.build.id.to_string());
    std::fs::create_dir_all(&local_dir).expect("本地目录");
    std::fs::write(local_dir.join("legacy.bin"), b"local").expect("本地正文");
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, job_id, attempt, backend, state, name, path, size, sha256,
             created_at, retention_until)
         VALUES (?, ?, 1, 'local', 'ready', 'legacy.bin', ?, 5, 'def', 1, 2)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .bind(format!("{}/legacy.bin", h.build.id))
    .execute(&h.app.pool)
    .await
    .expect("本地元数据");
    sqlx::query(
        "INSERT INTO logs
            (build_id, job_id, attempt, start_seq, end_seq, step, stream, data, created_at)
         VALUES (?, ?, 1, 0, 0, -1, '', X'1f8b', 1)",
    )
    .bind(h.build.id)
    .bind(h.job_a)
    .execute(&h.app.pool)
    .await
    .expect("日志");
    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Succeeded,
                3
            )
            .await
            .expect("终态")
    );

    let resp = common::req_with_cookie(
        &h.app,
        "DELETE",
        "/api/v1/projects/demo",
        None,
        Some(&h.cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let accepted = common::body_json(resp).await;
    assert_eq!(accepted["scope"], "project");
    assert_eq!(accepted["project_id"], project_id);
    assert_eq!(accepted["project_name"], "demo", "冻结后仍携带项目名快照");

    let agent_write = agent_post_json(
        &h,
        &format!("/api/v1/agent/artifacts/{}/dist.bin/upload-url", h.job_a),
        r#"{"size":1}"#,
    )
    .await;
    assert_eq!(
        agent_write.status(),
        StatusCode::NOT_FOUND,
        "项目冻结后旧 Agent token 不得继续写入该项目空间"
    );

    assert_eq!(
        common::req_with_cookie(
            &h.app,
            "GET",
            "/api/v1/projects/demo",
            None,
            Some(&h.cookie),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "受理后立即冻结项目访问"
    );
    let status = common::body_json(
        common::req_with_cookie(
            &h.app,
            "GET",
            "/api/v1/project-deletions",
            None,
            Some(&h.cookie),
        )
        .await,
    )
    .await;
    assert_eq!(status["items"][0]["state"], "queued");
    assert_eq!(status["items"][0]["project_name"], "demo");

    h.mock.fail_deletes.store(true, Ordering::Relaxed);
    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("项目清理")
    );
    let status = common::body_json(
        common::req_with_cookie(
            &h.app,
            "GET",
            "/api/v1/project-deletions",
            None,
            Some(&h.cookie),
        )
        .await,
    )
    .await;
    assert_eq!(status["items"][0]["state"], "failed");
    let deletion_id = status["items"][0]["id"].as_i64().expect("删除任务 id");

    h.mock.fail_deletes.store(false, Ordering::Relaxed);
    let retry = common::req_with_cookie(
        &h.app,
        "POST",
        &format!("/api/v1/project-deletions/{deletion_id}/retry"),
        Some("{}".into()),
        Some(&h.cookie),
    )
    .await;
    assert_eq!(retry.status(), StatusCode::ACCEPTED);
    assert_eq!(common::body_json(retry).await["state"], "queued");
    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("重试项目清理")
    );
    assert!(!h.mock.objects.lock().unwrap().contains_key(object_key));
    assert!(!local_dir.exists(), "旧本地产物目录已清理");
    let logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE build_id = ?")
        .bind(h.build.id)
        .fetch_one(&h.app.pool)
        .await
        .expect("日志数");
    assert_eq!(logs, 0);
    let lifecycle: String = sqlx::query_scalar("SELECT lifecycle FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_one(&h.app.pool)
        .await
        .expect("项目墓碑");
    assert_eq!(lifecycle, "deleted");
    let status = common::body_json(
        common::req_with_cookie(
            &h.app,
            "GET",
            "/api/v1/project-deletions",
            None,
            Some(&h.cookie),
        )
        .await,
    )
    .await;
    assert_eq!(status["items"][0]["state"], "completed");
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log
         WHERE event_type = 'project_deletion_requested' AND project_name = 'demo'",
    )
    .fetch_one(&h.app.pool)
    .await
    .expect("审计");
    assert_eq!(audit, 1);
}

/// #128：未配置 S3 的部署也能完成项目清理；空远端对象集不应被当作配置错误。
#[tokio::test]
async fn project_delete_without_s3_completes_local_cleanup() {
    let mut h = harness().await;
    h.app.state.s3 = None;
    let project_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(&h.app.pool)
        .await
        .expect("项目 id");
    let builds = BuildRepo::new(h.app.pool.clone());
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Running,
                2
            )
            .await
            .expect("运行态")
    );
    assert!(
        builds
            .transition(
                h.build.id,
                sisyphus_server::store::builds::BuildStatus::Succeeded,
                3
            )
            .await
            .expect("终态")
    );
    h.app
        .state
        .deletions
        .enqueue_project(project_id, "admin")
        .await
        .expect("项目删除入队");

    assert!(
        sisyphus_server::deletion::run_once(&h.app.state)
            .await
            .expect("本地项目清理")
    );
    let state = h
        .app
        .state
        .deletions
        .list_project_deletions()
        .await
        .expect("删除状态");
    assert_eq!(
        state[0].state,
        sisyphus_server::store::deletions::DeletionState::Completed
    );
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

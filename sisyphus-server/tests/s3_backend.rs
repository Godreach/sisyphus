//! 票 #122：S3 启动校验与连接自检，覆盖错误凭据、缺失 bucket、误换配置。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use http_body_util::BodyExt;
use sisyphus_server::config::S3Config;
use sisyphus_server::storage::{S3Client, StorageError, prepare_s3};
use sqlx::SqlitePool;

#[derive(Clone)]
struct Mock {
    access_key: String,
    bucket: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    uploads: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

async fn handle(State(mock): State<Mock>, req: Request<axum::body::Body>) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !auth.contains(&format!("Credential={}/", mock.access_key)) {
        return (
            StatusCode::FORBIDDEN,
            "<Error><Code>InvalidAccessKeyId</Code><Message>denied</Message></Error>",
        )
            .into_response();
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();

    let Some(rest) = path
        .strip_prefix(&format!("/{}", mock.bucket))
        .or_else(|| path.strip_prefix(&format!("/{}/", mock.bucket)).map(|_| ""))
    else {
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

    if query.contains("uploads")
        && method == axum::http::Method::POST
        && !query.contains("uploadId")
    {
        let id = format!("up-{}", mock.uploads.lock().unwrap().len() + 1);
        mock.uploads.lock().unwrap().insert(id.clone(), Vec::new());
        return (
            StatusCode::OK,
            format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"),
        )
            .into_response();
    }
    if let Some(upload_id) = query_val(&query, "uploadId") {
        if method == axum::http::Method::DELETE {
            mock.uploads.lock().unwrap().remove(&upload_id);
            return StatusCode::NO_CONTENT.into_response();
        }
        if method == axum::http::Method::PUT {
            let mut data = body.to_vec();
            if let Some(src) = headers
                .get("x-amz-copy-source")
                .and_then(|v| v.to_str().ok())
            {
                let src_key = src.rsplit('/').next().unwrap_or("");
                data = mock
                    .objects
                    .lock()
                    .unwrap()
                    .get(src_key)
                    .cloned()
                    .unwrap_or_default();
            }
            mock.uploads.lock().unwrap().insert(upload_id, data);
            let mut resp = StatusCode::OK.into_response();
            resp.headers_mut()
                .insert("etag", "\"etag-1\"".parse().unwrap());
            *resp.body_mut() =
                axum::body::Body::from("<CopyPartResult><ETag>\"etag-1\"</ETag></CopyPartResult>");
            return resp;
        }
        if method == axum::http::Method::POST {
            let data = mock
                .uploads
                .lock()
                .unwrap()
                .remove(&upload_id)
                .unwrap_or_default();
            mock.objects.lock().unwrap().insert(key.to_string(), data);
            return StatusCode::OK.into_response();
        }
    }

    match method {
        m if m == axum::http::Method::PUT => {
            if let Some(src) = headers
                .get("x-amz-copy-source")
                .and_then(|v| v.to_str().ok())
            {
                let src_key = src.rsplit('/').next().unwrap_or("");
                let data = mock
                    .objects
                    .lock()
                    .unwrap()
                    .get(src_key)
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
            if mock.objects.lock().unwrap().contains_key(key) {
                StatusCode::OK.into_response()
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

fn query_val(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        (k == name && !v.is_empty()).then(|| v.to_string())
    })
}

async fn spawn_mock(access_key: &str, bucket: &str) -> (SocketAddr, Mock) {
    let mock = Mock {
        access_key: access_key.into(),
        bucket: bucket.into(),
        objects: Arc::new(Mutex::new(HashMap::new())),
        uploads: Arc::new(Mutex::new(HashMap::new())),
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

async fn pool() -> (tempfile::TempDir, SqlitePool) {
    let dir = tempfile::tempdir().expect("tmp");
    sisyphus_server::config::Config::load(
        dir.path().to_path_buf(),
        sisyphus_server::config::Overrides::default(),
        sisyphus_server::config::Overrides::default(),
    )
    .expect("layout");
    let pool = sisyphus_server::store::bootstrap(dir.path())
        .await
        .expect("bootstrap");
    (dir, pool)
}

#[tokio::test]
async fn prepare_s3_rejects_wrong_credentials() {
    let (addr, _mock) = spawn_mock("good-key", "sisy").await;
    let (_dir, pool) = pool().await;
    let cfg = s3_cfg(addr, "sisy", "bad-key", "secret");
    let err = prepare_s3(&pool, Some(&cfg))
        .await
        .expect_err("错误凭据应失败");
    assert!(matches!(err, StorageError::Credentials(_)), "{err}");
    assert!(!err.to_string().contains("secret"), "{err}");
}

#[tokio::test]
async fn prepare_s3_rejects_missing_bucket() {
    let (addr, _mock) = spawn_mock("good-key", "sisy").await;
    let (_dir, pool) = pool().await;
    let cfg = s3_cfg(addr, "no-such", "good-key", "secret");
    let err = prepare_s3(&pool, Some(&cfg))
        .await
        .expect_err("缺失 bucket 应失败");
    assert!(matches!(err, StorageError::MissingBucket(_)), "{err}");
}

#[tokio::test]
async fn prepare_s3_records_identity_and_rejects_replacement_with_objects() {
    let (addr, _mock) = spawn_mock("good-key", "old").await;
    let (_dir, pool) = pool().await;
    let cfg = s3_cfg(addr, "old", "good-key", "secret");
    prepare_s3(&pool, Some(&cfg)).await.expect("首次应成功");

    sqlx::query(
        "INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at)
         VALUES ('demo', 'git', 'https://example.com/r', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at)
         VALUES (1, 'release', 1, 'succeeded', 'manual', '{}', 1, '{}', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifacts
            (build_id, name, path, size, sha256, created_at, retention_until, backend, state)
         VALUES (1, 'remote.bin', 'objects/remote.bin', 3, 'abc', 0, 1, 's3', 'ready')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (addr2, _mock2) = spawn_mock("good-key", "new").await;
    let cfg2 = s3_cfg(addr2, "new", "good-key", "secret");
    let err = prepare_s3(&pool, Some(&cfg2))
        .await
        .expect_err("误换配置应拒启");
    let msg = err.to_string();
    assert!(msg.contains("不一致"), "{msg}");
    assert!(!msg.contains("secret"), "{msg}");
}

#[tokio::test]
async fn test_connection_covers_required_ops_and_cleans_up() {
    let (addr, mock) = spawn_mock("good-key", "sisy").await;
    let cfg = s3_cfg(addr, "sisy", "good-key", "secret");
    let client = S3Client::new(&cfg).expect("client");
    let report = client.test_connection().await;
    assert!(
        report.ok,
        "{:?}",
        report.checks.iter().filter(|c| !c.ok).collect::<Vec<_>>()
    );
    let ops: Vec<_> = report.checks.iter().map(|c| c.op.as_str()).collect();
    for need in [
        "put",
        "head",
        "range_get",
        "copy",
        "multipart_upload",
        "multipart_copy",
        "delete",
    ] {
        assert!(ops.contains(&need), "缺少 {need}：{ops:?}");
    }
    assert!(
        mock.objects.lock().unwrap().is_empty(),
        "探针应清理：{:?}",
        mock.objects.lock().unwrap().keys()
    );
}

#[tokio::test]
async fn unconfigured_prepare_is_none() {
    let (_dir, pool) = pool().await;
    let got = prepare_s3(&pool, None).await.expect("未配置应成功");
    assert!(got.is_none());
}

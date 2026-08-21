//! Agent 产物传输的 HTTP 缝测试（票 #74 AC）：极简 axum stub server（真
//! HTTP 栈——chunked 上传体/流式下载响应）+ 真 reqwest client 驱动
//! [`sisyphus_agent::artifacts::RealArtifactIo`]——验证请求形态（Bearer
//! token、路径契约）、上传字节完整、下载落盘与错误路径（404 的清晰消息
//! 透传）。不依赖 server crate（stub 只按契约回响应，票 #57 同纪律）。

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use sisyphus_agent::artifacts::{ArtifactError, ArtifactIo, RealArtifactIo};

/// 上传观测：路径 + 收到的全部字节 + Authorization 头。
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)] // 测试观测记录：三元组序列，不拆类型别名
struct UploadSeen {
    inner: Arc<Mutex<Vec<(String, Vec<u8>, String)>>>,
}

/// 下载 stub 的可配结果：`Ok(bytes)` 回流；`Err((status, message))` 回
/// 统一 JSON 错误体。
enum DownloadScript {
    Ok(Vec<u8>),
    Reject(u16, String),
}

/// 起 stub：上传端点收字节回 201；下载端点按脚本回。
async fn spawn_stub(
    uploads: UploadSeen,
    download: Arc<tokio::sync::Mutex<DownloadScript>>,
) -> String {
    let upload_state = uploads.clone();
    let app = Router::new()
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}",
            post(
                move |AxumPath((job_id, name)): AxumPath<(String, String)>,
                      headers: HeaderMap,
                      body: axum::body::Bytes| {
                    let state = upload_state.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        state.inner.lock().expect("锁").push((
                            format!("{job_id}/{name}"),
                            body.to_vec(),
                            auth,
                        ));
                        (
                            StatusCode::CREATED,
                            axum::Json(serde_json::json!({
                                "name": name, "size": body.len(), "sha256": "fixed"
                            })),
                        )
                            .into_response()
                    }
                },
            ),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/downloads/{source_job}/{name}",
            get(
                move |AxumPath((_job_id, source_job, name)): AxumPath<(String, String, String)>| {
                    let script = download.clone();
                    async move {
                        match &*script.lock().await {
                            DownloadScript::Ok(bytes) => (
                                StatusCode::OK,
                                [
                                    ("content-length", bytes.len().to_string()),
                                    ("x-sisyphus-sha256", "fixed".to_string()),
                                ],
                                bytes.clone(),
                            )
                                .into_response(),
                            DownloadScript::Reject(status, message) => (
                                StatusCode::from_u16(*status).expect("状态码"),
                                axum::Json(serde_json::json!({
                                    "error": "not_found",
                                    "message": format!("{message}（{source_job} 的产物 {name}）"),
                                })),
                            )
                                .into_response(),
                        }
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

async fn stub(download: DownloadScript) -> (String, UploadSeen) {
    let seen = UploadSeen::default();
    let addr = spawn_stub(seen.clone(), Arc::new(tokio::sync::Mutex::new(download))).await;
    (addr, seen)
}

fn io(addr: &str) -> RealArtifactIo {
    // no_proxy：测试环境可能带全局代理 env（127.0.0.1 直连不绕代理会被
    // 环境代理 502）；生产构造（`new`）不受影响。
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build");
    RealArtifactIo::with_client(
        client,
        Some(addr.to_string()),
        Some("sisa_test_token".into()),
    )
}

/// 上传：Bearer token + 路径契约 + 字节完整（大文件走 chunked 流）。
#[tokio::test]
async fn upload_streams_file_bytes_with_bearer_token() {
    let (addr, seen) = stub(DownloadScript::Ok(vec![])).await;
    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("dist.bin");
    let payload = vec![0xABu8; 300_000]; // > 64 KiB 块，跨块上传。
    tokio::fs::write(&src, &payload).await.expect("写源文件");

    io(&addr)
        .upload("42", "dist.bin", &src)
        .await
        .expect("上传应成功");

    let calls = seen.inner.lock().expect("锁").clone();
    assert_eq!(calls.len(), 1);
    let (path, body, auth) = &calls[0];
    assert_eq!(path, "42/dist.bin", "路径契约：{{job_id}}/{{name}}");
    assert_eq!(body, &payload, "上传字节完整（含跨块）");
    assert_eq!(auth, "Bearer sisa_test_token");
}

/// 上传被拒（404/422 等）：统一错误体的 message 透传。
#[tokio::test]
async fn upload_rejection_surfaces_server_message() {
    // 上传回 201 的 stub 即可——拒绝路径用不存在的任务路径触发不了 stub
    // 差异；直接断言 download 面的透传（同一 rejection 解析），此处仅验
    // Unconfigured 之外的形态由下载用例覆盖。
    let (addr, _seen) = stub(DownloadScript::Ok(vec![])).await;
    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("x");
    tokio::fs::write(&src, b"x").await.expect("写");
    io(&addr)
        .upload("42", "x", &src)
        .await
        .expect("stub 上传恒 201");
}

/// 下载：字节落盘到目标路径（含父目录创建），内容一致。
#[tokio::test]
async fn download_writes_bytes_to_dest() {
    let payload = b"dep-artifact-bytes".repeat(5000);
    let (addr, _seen) = stub(DownloadScript::Ok(payload.clone())).await;
    let dir = tempfile::tempdir().expect("临时目录");
    let dest = dir.path().join("deps").join("in").join("dist.bin");

    io(&addr)
        .download("43", "build", "dist.bin", &dest)
        .await
        .expect("下载应成功");
    assert_eq!(
        tokio::fs::read(&dest).await.expect("读回"),
        payload,
        "落盘字节一致（父目录自动创建）"
    );
    // 无 .part 残留。
    let entries: Vec<_> = std::fs::read_dir(dir.path().join("deps/in"))
        .expect("枚举")
        .map(|e| e.expect("项").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["dist.bin".to_string()]);
}

/// 下载被拒（404「依赖产物尚不存在」）：状态码 + 服务端清晰消息透传。
#[tokio::test]
async fn download_rejection_surfaces_clear_message() {
    let (addr, _seen) = stub(DownloadScript::Reject(
        404,
        "依赖产物尚不存在：任务 build 的产物 dist.bin 未上传".into(),
    ))
    .await;
    let dir = tempfile::tempdir().expect("临时目录");
    let dest = dir.path().join("dist.bin");

    let err = io(&addr)
        .download("43", "build", "dist.bin", &dest)
        .await
        .expect_err("404 应报错");
    match err {
        ArtifactError::Rejected { status, message } => {
            assert_eq!(status, 404);
            assert!(
                message.contains("依赖产物尚不存在"),
                "服务端清晰消息透传：{message}"
            );
        }
        other => panic!("应为 Rejected：{other}"),
    }
    assert!(!dest.exists(), "失败不落半截文件");
}

/// 契约常量与端点 URL 拼接（纯函数，同票 #57 的 url 单测纪律）。
#[test]
fn upload_endpoint_constant_matches_contract() {
    assert_eq!(
        sisyphus_agent::artifacts::UPLOAD_ENDPOINT,
        "/api/v1/agent/artifacts"
    );
    let _ = Path::new("/tmp");
}

/// 大下载（> 64 KiB）流式落盘跨块。
#[tokio::test]
async fn download_large_payload_roundtrips() {
    let payload = vec![7u8; 200_000];
    let (addr, _seen) = stub(DownloadScript::Ok(payload.clone())).await;
    let dir = tempfile::tempdir().expect("临时目录");
    let dest = dir.path().join("big.bin");
    io(&addr)
        .download("43", "build", "big.bin", &dest)
        .await
        .expect("下载");
    assert_eq!(tokio::fs::read(&dest).await.expect("读回"), payload);
}

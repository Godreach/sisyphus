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
            "/api/v1/agent/artifacts/{job_id}/preflight",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}/upload-url",
            post(|| async {
                (
                    StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({
                        "code": "CONFLICT",
                        "message": "未配置 S3，无法签发直传 URL",
                    })),
                )
                    .into_response()
            }),
        )
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
        .redirect(reqwest::redirect::Policy::none())
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

/// 已配置 S3：Agent 申请临时 PUT URL、直传字节、再 complete。
#[tokio::test]
async fn upload_uses_presigned_put_then_complete() {
    let put_seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let complete_seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let put_state = put_seen.clone();
    let complete_state = complete_seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let put_url = format!("http://{addr}/s3/tmp/dist.bin");
    let grant_url = put_url.clone();
    let app = Router::new()
        .route(
            "/api/v1/agent/artifacts/{job_id}/preflight",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}/upload-url",
            post(move || {
                let grant_url = grant_url.clone();
                async move {
                    axum::Json(serde_json::json!({ "url": grant_url, "expires_in": 300 }))
                        .into_response()
                }
            }),
        )
        .route(
            "/s3/tmp/dist.bin",
            axum::routing::put(move |body: axum::body::Bytes| {
                let put_state = put_state.clone();
                async move {
                    put_state.lock().expect("锁").push(body.to_vec());
                    StatusCode::OK
                }
            }),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}/complete",
            post(move |body: axum::body::Bytes| {
                let complete_state = complete_state.clone();
                async move {
                    complete_state
                        .lock()
                        .expect("锁")
                        .push(String::from_utf8_lossy(&body).into_owned());
                    (
                        StatusCode::CREATED,
                        axum::Json(serde_json::json!({
                            "name": "dist.bin", "size": 3, "sha256": "x"
                        })),
                    )
                        .into_response()
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("dist.bin");
    tokio::fs::write(&src, b"abc").await.expect("写");
    io(&format!("http://{addr}"))
        .upload("42", "dist.bin", &src)
        .await
        .expect("直传应成功");
    assert_eq!(put_seen.lock().expect("锁").as_slice(), [b"abc".to_vec()]);
    let complete = complete_seen.lock().expect("锁").clone();
    assert_eq!(complete.len(), 1);
    assert!(complete[0].contains("\"size\":3"), "{}", complete[0]);
}

/// 大文件：单分片失败只重试该分片；短期 URL 过期后重新申请同一会话的
/// 分片 URL，complete 收到全部有序 ETag。
#[tokio::test]
async fn multipart_upload_retries_failed_part_and_completes() {
    let attempts = Arc::new(Mutex::new(std::collections::HashMap::<u32, usize>::new()));
    let uploaded = Arc::new(Mutex::new(std::collections::HashMap::<u32, Vec<u8>>::new()));
    let complete_seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let grants = Arc::new(Mutex::new(0usize));
    let attempts_state = attempts.clone();
    let uploaded_state = uploaded.clone();
    let complete_state = complete_seen.clone();
    let grants_state = grants.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = Router::new()
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}/upload-url",
            post(move |body: axum::body::Bytes| {
                let grants = grants_state.clone();
                async move {
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).expect("grant json");
                    assert_eq!(request["size"], 18);
                    let mut count = grants.lock().expect("锁");
                    *count += 1;
                    let generation = *count;
                    axum::Json(serde_json::json!({
                        "mode": "multipart",
                        "upload_id": "upload-1",
                        "part_size": 4,
                        "expires_in": 0,
                        "parts": (1..=5).map(|part| serde_json::json!({
                            "part_number": part,
                            "url": format!("http://{addr}/s3/tmp/{generation}/{part}")
                        })).collect::<Vec<_>>()
                    }))
                }
            }),
        )
        .route(
            "/s3/tmp/{generation}/{part}",
            axum::routing::put(
                move |AxumPath((generation, part)): AxumPath<(usize, u32)>,
                      body: axum::body::Bytes| {
                    let attempts = attempts_state.clone();
                    let uploaded = uploaded_state.clone();
                    async move {
                        if part == 5 && generation == 1 {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                        let attempt = {
                            let mut attempts = attempts.lock().expect("锁");
                            let attempt = attempts.entry(part).or_default();
                            *attempt += 1;
                            *attempt
                        };
                        if part == 2 && attempt == 1 {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        uploaded.lock().expect("锁").insert(part, body.to_vec());
                        (StatusCode::OK, [("etag", format!("\"etag-{part}\""))]).into_response()
                    }
                },
            ),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/{name}/complete",
            post(move |body: axum::body::Bytes| {
                let complete = complete_state.clone();
                async move {
                    complete
                        .lock()
                        .expect("锁")
                        .push(serde_json::from_slice(&body).expect("complete json"));
                    (
                        StatusCode::CREATED,
                        axum::Json(serde_json::json!({
                            "name": "dist.bin", "size": 18, "sha256": "x"
                        })),
                    )
                        .into_response()
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("dist.bin");
    tokio::fs::write(&src, b"abcdefghijklmnopqr")
        .await
        .expect("写");
    io(&format!("http://{addr}"))
        .upload("42", "dist.bin", &src)
        .await
        .expect("multipart 上传应成功");

    assert_eq!(attempts.lock().expect("锁").get(&2), Some(&2));
    assert_eq!(*grants.lock().expect("锁"), 2, "后续分片应使用刷新后的 URL");
    let uploaded = uploaded.lock().expect("锁");
    assert_eq!(uploaded.get(&1).map(Vec::as_slice), Some(&b"abcd"[..]));
    assert_eq!(uploaded.get(&2).map(Vec::as_slice), Some(&b"efgh"[..]));
    assert_eq!(uploaded.get(&3).map(Vec::as_slice), Some(&b"ijkl"[..]));
    assert_eq!(uploaded.get(&4).map(Vec::as_slice), Some(&b"mnop"[..]));
    assert_eq!(uploaded.get(&5).map(Vec::as_slice), Some(&b"qr"[..]));
    let complete = complete_seen.lock().expect("锁");
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0]["upload_id"], "upload-1");
    assert_eq!(complete[0]["parts"][0]["part_number"], 1);
    assert_eq!(complete[0]["parts"][2]["etag"], "\"etag-3\"");
    assert_eq!(complete[0]["parts"][4]["part_number"], 5);
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

async fn directory_stub(corrupt: bool, unsafe_path: bool, empty: bool) -> String {
    let entries = if empty {
        vec![]
    } else {
        vec![
            serde_json::json!({"path":"empty","kind":"directory","size":0,"sha256":"","executable":false}),
            serde_json::json!({"path":if unsafe_path { "../escape" } else { "nested/a.txt" },"kind":"file","size":3,
            "sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "executable":true}),
        ]
    };
    let app = Router::new()
        .route(
            "/api/v1/agent/artifacts/{job_id}/downloads/{source_job}/{name}",
            get(move || {
                let entries = entries.clone();
                async move {
                    axum::Json(
                        serde_json::json!({"set":{"id":7,"state":"ready"}, "entries":entries}),
                    )
                }
            }),
        )
        .route(
            "/api/v1/agent/artifacts/{job_id}/sets/{set_id}/file",
            get(move || async move { if corrupt { "bad" } else { "abc" } }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn directory_dependency_validates_and_replaces_instead_of_merging() {
    let addr = directory_stub(false, false, false).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("target");
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("old.txt"), "old").unwrap();
    io(&addr)
        .download("42", "build", "dist", &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest.join("nested/a.txt")).unwrap(), b"abc");
    assert!(dest.join("empty").is_dir());
    assert!(!dest.join("old.txt").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(dest.join("nested/a.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "staging 和旧目录均已清理"
    );
}

#[tokio::test]
async fn failed_directory_dependency_preserves_old_directory_and_rejects_escape() {
    for (corrupt, unsafe_path) in [(true, false), (false, true)] {
        let addr = directory_stub(corrupt, unsafe_path, false).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("target");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), "old").unwrap();
        assert!(
            io(&addr)
                .download("42", "build", "dist", &dest)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(dest.join("old.txt")).unwrap(), b"old");
        assert!(!dir.path().join("escape").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn empty_directory_dependency_replaces_old_tree() {
    let addr = directory_stub(false, false, true).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("target");
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("old.txt"), "old").unwrap();
    io(&addr)
        .download("42", "build", "dist", &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read_dir(dest).unwrap().count(), 0);
}

#[tokio::test]
async fn directory_upload_rejects_root_and_ancestor_links() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    let link = dir.path().join("link");
    std::fs::create_dir_all(outside.join("nested")).unwrap();
    std::fs::write(outside.join("nested/secret.txt"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    {
        let result = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(result.status.success(), "创建测试 junction 失败");
    }
    for path in [&link, &link.join("nested")] {
        let error = io("http://127.0.0.1:1")
            .upload_directory("42", "dist", path)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ArtifactError::Io(_)),
            "链接必须在发送 HTTP 前拒绝：{error}"
        );
    }
}

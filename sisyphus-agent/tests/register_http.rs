//! Agent 注册调用的 HTTP 缝测试（票 #57 AC）：dev-deps 极简 HTTP stub
//! （tokio TcpListener 手写 HTTP/1.1 响应）+ 真 reqwest client 驱动
//! `sisyphus_agent::register::register`——验证请求形态（POST /api/v1/
//! agent/register、JSON body 含 name + register_code）、token 落盘
//! （0600 + 读回）与错误路径（无效 404 / 已用 409 / 过期 403 / 停用 403 /
//! 网络不可达）。不依赖 server crate（stub 只按契约回固定响应）。
//!
//! 形态基准：agent 侧 B3-T1 的 fake Server 集成测试（dev-deps 手写对端）。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use sisyphus_agent::register::{RegisterError, persist_token, register};
use sisyphus_agent::config::TOKEN_FILE_NAME;

/// 极简 HTTP/1.1 stub：单连接读请求行 + 头（收 body 长度）→ 按脚本回响应。
/// 记录收到的请求行与 body（断言请求形态用）。
struct Stub {
    addr: std::net::SocketAddr,
    // (请求行, body) 观测序列。
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

/// 单响应脚本：状态行 + body（`Connection: close` 短连，免 keep-alive 复杂度）。
fn response(status_line: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

impl Stub {
    /// 起一个 stub：每连回 `respond`（参数 = 收到的请求体，返回完整响应）。
    fn spawn<F>(mut respond: F) -> Self
    where
        F: FnMut(&str) -> String + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let seen_arc = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    break;
                }
                // 头（读空行止）。
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some(v) = line
                        .to_ascii_lowercase()
                        .strip_prefix("content-length:")
                    {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                let body = String::from_utf8_lossy(&body).into_owned();
                seen_arc
                    .lock()
                    .expect("锁")
                    .push((request_line.trim().to_string(), body.clone()));
                let response = respond(&body);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { addr, seen }
    }
}

/// 构建真 reqwest client（与生产同构造）。
fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn register_posts_name_and_code_and_returns_token() {
    let stub = Stub::spawn(|_body| {
        response("200 OK", r#"{"token":"sisa_newtoken_abc123"}"#)
    });
    let token = register(
        &client(),
        &format!("http://{}", stub.addr),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect("兑码成功");

    assert_eq!(token, "sisa_newtoken_abc123");
    // 请求形态：POST /api/v1/agent/register + JSON body（name + register_code）。
    let seen = stub.seen.lock().expect("锁");
    let (request_line, body) = &seen[0];
    assert_eq!(request_line, "POST /api/v1/agent/register HTTP/1.1");
    let json: serde_json::Value = serde_json::from_str(body).expect("body 为 JSON");
    assert_eq!(json["name"], "linux-1");
    assert_eq!(json["register_code"], "sisa_reg_deadbeef");
}

#[tokio::test]
async fn register_rejects_non_success_with_server_message() {
    // 已用 409：message 从统一错误体里带出。
    let stub = Stub::spawn(|_| {
        response(
            "409 Conflict",
            r#"{"code":"CONFLICT","message":"注册码已使用（一次性，兑完作废）"}"#,
        )
    });
    let err = register(
        &client(),
        &format!("http://{}", stub.addr),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect_err("已用应拒绝");
    match &err {
        RegisterError::Rejected { status, message } => {
            assert_eq!(*status, 409);
            assert!(message.contains("已使用"), "message: {message}");
        }
        other => panic!("应 Rejected，实际 {other:?}"),
    }

    // 停用/过期 403：同形态。
    let stub = Stub::spawn(|_| {
        response(
            "403 Forbidden",
            r#"{"code":"FORBIDDEN","message":"Agent 已停用，无法注册"}"#,
        )
    });
    let err = register(
        &client(),
        &format!("http://{}", stub.addr),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect_err("停用应拒绝");
    assert!(matches!(
        err,
        RegisterError::Rejected { status: 403, .. }
    ));

    // 无效 404。
    let stub = Stub::spawn(|_| {
        response("404 Not Found", r#"{"code":"NOT_FOUND","message":"注册码无效"}"#)
    });
    let err = register(
        &client(),
        &format!("http://{}", stub.addr),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect_err("无效应拒绝");
    assert!(matches!(
        err,
        RegisterError::Rejected { status: 404, .. }
    ));

    // 非 JSON 错误体：message 落状态码文本兜底。
    let stub = Stub::spawn(|_| response("500 Internal Server Error", "oops"));
    let err = register(
        &client(),
        &format!("http://{}", stub.addr),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect_err("500 应拒绝");
    assert!(matches!(
        err,
        RegisterError::Rejected { status: 500, .. }
    ));
}

#[tokio::test]
async fn register_surfaces_network_failure() {
    // 不可达地址（未监听端口）：Network 错误。
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener); // 立即释放，端口无人监听
    let err = register(
        &client(),
        &format!("http://{addr}"),
        "linux-1",
        "sisa_reg_deadbeef",
    )
    .await
    .expect_err("不可达应报错");
    assert!(
        matches!(err, RegisterError::Network(_)),
        "应 Network，实际 {err:?}"
    );
}

#[tokio::test]
async fn register_rejects_malformed_success_body() {
    // 200 但 token 缺失/形态非法：Response 错误。
    for body in [
        r#"{}"#,
        r#"{"token":""}"#,
        r#"{"token":"not_sisa"}"#,
        "not-json",
    ] {
        let stub = Stub::spawn(move |_| response("200 OK", body));
        let err = register(
            &client(),
            &format!("http://{}", stub.addr),
            "linux-1",
            "sisa_reg_deadbeef",
        )
        .await
        .expect_err("形态非法应报错");
        assert!(
            matches!(err, RegisterError::Response(_)),
            "{body} 应 Response 错误，实际 {err:?}"
        );
    }
}

#[test]
fn persist_token_writes_readable_0600_file() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    persist_token(dir.path(), "sisa_token_xyz").expect("落盘");
    let path = dir.path().join(TOKEN_FILE_NAME);
    assert_eq!(
        std::fs::read_to_string(&path).expect("读回").trim(),
        "sisa_token_xyz"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "token 文件应为 0600");
    }
}

/// 票 #57 AC「落盘后直连不需要注册码」的组合缝：兑码 token 落盘后，
/// 后续启动经 `Agent::new` 读 token 的同一路径（`config::read_token`）直接
/// 取到该凭据——`--reg-key` 只需一次引导，落盘即直连凭据。
#[test]
fn persisted_token_feeds_direct_connect_without_reg_key() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let token = "sisa_token_direct";
    persist_token(dir.path(), token).expect("落盘");

    // Agent::new 的取凭据路径（lib.rs：config::read_token(&config.data_dir)）。
    assert_eq!(
        sisyphus_agent::config::read_token(dir.path()).as_deref(),
        Some(token),
        "落盘 token 即后续直连凭据（无需再带 --reg-key）"
    );
}

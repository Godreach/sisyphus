//! B5-T4（票 #76）Agent 升级面集成测试：真实 tonic gRPC 通道 + 真实 store +
//! oneshot REST（与二进制同组合根）。fake Agent 经 proto 客户端驱动，闭环验证：
//!
//! - 包上传：文件名解析、窗口外拒收（409）、sha256 记录；多包。
//! - 包下载：agent token 鉴权（PAT/无凭据/停用 401）、sha256 响应头。
//! - 完整升级往返：上传 → 指令 → 排空 → 下载校验 → 版本上报更新。
//! - 过旧 Agent：任务面拒连（match_candidates 排除）+ 升级面保留（指令仍送达）。
//! - 工作区/缓存：列表经通道往返、删除指令下发。
//! - 全量升级受理摘要：issued/skipped。
//!
//! 形态基准：`grpc_auth.rs`（真实 tonic + mpsc 驱动客户端）+ `common`（oneshot
//! REST + admin cookie）。REST 与 gRPC 共享同一 `AppState`（含 `agent_sessions`），
//! 故 REST 升级指令经通道送达 fake Agent。

use std::time::Duration;

use sisyphus_proto::agent::{
    ChannelMessage, Handshake, Version,
    agent_channel_client::AgentChannelClient,
    cache_command::Kind as CacheKind,
    channel_message::Kind,
    workspace_command::Kind as WorkspaceKind,
};
use sisyphus_server::auth::{TokenFamily, generate_token, token_hash};
use sisyphus_server::sched::SchedulerHandle;
use sisyphus_server::store::agents::{AgentVersion, NewAgent};
use sisyphus_server::grpc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

mod common;
use common::{TestApp, body_json, body_text, custom_req, req_with_cookie, setup_and_login, test_app};

/// 升级包下载相对路径（与后端 `upgrade_download_url` 一致）。
fn download_path(package_name: &str) -> String {
    format!("/api/v1/agent/upgrade-packages/{package_name}")
}

/// 起真实 tonic gRPC 服务（共享 TestApp 的 state.agent_sessions），返回监听地址。
async fn spawn_grpc(app: &TestApp) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = app.state.clone();
    let sessions = app.state.agent_sessions.clone();
    // 升级往返不驱动调度循环（REST 直接经 agent_sessions 下发；重连补发面
    // 不在此用例）：丢弃面句柄。
    let scheduler = SchedulerHandle::discard();
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc::service(state, sessions, scheduler))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });
    (addr, handle)
}

/// 建 Agent 条目（token 哈希落库）+ 返回明文 token。
async fn create_agent(app: &TestApp, name: &str) -> String {
    let token = generate_token(TokenFamily::Agent);
    let code = sisyphus_server::auth::generate_register_code();
    app.state
        .agents
        .create(NewAgent {
            name: name.into(),
            token_hash: token_hash(&token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建条目");
    token
}

/// 与 Server 建立连接：握手经请求流（mpsc 发送器驱动）发送，返回
/// (响应流, 上行发送器)。`version` 为 Agent 上报版本。
async fn connect(
    addr: std::net::SocketAddr,
    token: &str,
    version: Version,
) -> (tonic::Streaming<ChannelMessage>, mpsc::Sender<ChannelMessage>) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("tcp connect");
    let mut client = AgentChannelClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
    tx.send(ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(version),
            agent_name: "fake-agent".into(),
        })),
    })
    .await
    .expect("send handshake");
    let mut request = tonic::Request::new(ReceiverStream::new(rx));
    request.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}")).expect("值"),
    );
    let response = client.connect(request).await.expect("连接应成功");
    (response.into_inner(), tx)
}

/// 读完首帧握手回执（断言形态）。
async fn recv_handshake(stream: &mut tonic::Streaming<ChannelMessage>) {
    let msg = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("握手回执超时")
        .expect("读帧")
        .expect("帧");
    assert!(matches!(msg.kind, Some(Kind::Handshake(_))), "首帧应为握手回执");
}

/// 读首个非握手下行帧（命令），5s 超时。仅在「已触发下发」后调用（REST 升级 /
/// 列表查询后）——握手回执须先经 [`recv_handshake`] 消费，否则本函数会阻塞等
/// 下一帧。
async fn recv_cmd(stream: &mut tonic::Streaming<ChannelMessage>) -> Option<ChannelMessage> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Ok(Some(msg)) => {
                    if !matches!(msg.kind, Some(Kind::Handshake(_))) {
                        return Some(msg);
                    }
                }
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// 等到谓词成立或超时（落库是服务端异步路径）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..60 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("条件在 6s 内未成立");
}

fn v(major: u32, minor: u32, patch: u32) -> Version {
    Version { major, minor, patch }
}

/// admin 上传升级包（raw octet body + X-Sisyphus-Filename 头）。
async fn upload_pkg(app: &TestApp, cookie: &str, filename: &str, bytes: &str) -> axum::http::Response<axum::body::Body> {
    custom_req(
        app,
        "POST",
        "/api/v1/upgrade-packages",
        Some(bytes.into()),
        Some(cookie),
        &[
            ("sec-fetch-site", "same-origin".into()),
            ("x-sisyphus-filename", filename.into()),
        ],
        common::DEFAULT_PEER,
    )
    .await
}

// ===========================================================================
// 包上传 / 下载（REST 面，无需 Agent）
// ===========================================================================

#[tokio::test]
async fn package_upload_parses_filename_window_and_sha256() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 合法包（1.0.0 = Server 版本，窗口内）。
    let resp = upload_pkg(&app, &cookie, "sisyphus-agent-1.0.0-linux-x86_64.tar.gz", "fake-package-bytes-1.0.0").await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "窗口内应 201");
    let body = body_json(resp).await;
    assert_eq!(body["package_name"], "sisyphus-agent-1.0.0-linux-x86_64.tar.gz");
    assert_eq!(body["version"], serde_json::json!({ "major": 1, "minor": 0, "patch": 0 }));
    assert_eq!(body["target_os"], "linux");
    assert_eq!(body["target_arch"], "x86_64");
    assert_eq!(body["size"], 24);
    let sha = body["sha256"].as_str().expect("sha256").to_string();
    use sha2::{Digest, Sha256};
    assert_eq!(sha, format!("{:x}", Sha256::digest(b"fake-package-bytes-1.0.0")));

    // 窗口外：过旧（0.8.0 < N-1=0.9）→ 409。
    let resp = upload_pkg(&app, &cookie, "sisyphus-agent-0.8.0-linux-x86_64.tar.gz", "old").await;
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT, "过旧应 409");
    assert!(body_text(resp).await.contains("过旧"));

    // 窗口外：过新（1.1.0 > Server）→ 409。
    let resp = upload_pkg(&app, &cookie, "sisyphus-agent-1.1.0-linux-x86_64.tar.gz", "new").await;
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT, "过新应 409");
    assert!(body_text(resp).await.contains("过新"));

    // 文件名不可解析 → 422。
    let resp = upload_pkg(&app, &cookie, "agent-1.0.0-linux-x86_64.tar.gz", "x").await;
    assert_eq!(resp.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);

    // 多包：第二个目标三元组也上传成功（一次多包 = 连续多次上传）。
    let resp = upload_pkg(&app, &cookie, "sisyphus-agent-1.0.0-windows-x86_64.zip", "win-bytes").await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "多包第二份");

    // 列表含两份。
    let resp = req_with_cookie(&app, "GET", "/api/v1/upgrade-packages", None, Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(resp).await.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn package_download_requires_agent_token_and_returns_sha256_header() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let pkg = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
    upload_pkg(&app, &cookie, pkg, "download-bytes").await;
    let token = create_agent(&app, "linux-1").await;

    // 无凭据 → 401。
    let resp = common::get(&app, &download_path(pkg)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // PAT（sis_ 族）→ 401（仅 sisa_ 族放行）。
    let pat = sisyphus_server::auth::generate_token(TokenFamily::Pat);
    let resp = custom_req(&app, "GET", &download_path(pkg), None, None, &[("authorization", format!("Bearer {pat}"))], common::DEFAULT_PEER).await;
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "PAT 不进 agent 下载面");

    // Agent token（sisa_）→ 200 + sha256 头 + 字节体。
    let resp = custom_req(&app, "GET", &download_path(pkg), None, None, &[("authorization", format!("Bearer {token}"))], common::DEFAULT_PEER).await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    use sha2::{Digest, Sha256};
    let expect = format!("{:x}", Sha256::digest(b"download-bytes"));
    assert_eq!(resp.headers().get("x-sisyphus-sha256").unwrap().to_str().unwrap(), expect);
    assert_eq!(body_text(resp).await, "download-bytes");

    // 停用 Agent → 401。
    let id = app.state.agents.get_by_name("linux-1").await.unwrap().unwrap().id;
    app.state.agents.set_disabled(id, true).await.unwrap();
    let resp = custom_req(&app, "GET", &download_path(pkg), None, None, &[("authorization", format!("Bearer {token}"))], common::DEFAULT_PEER).await;
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED, "停用 Agent 不下载");
}

// ===========================================================================
// 完整升级往返（fake Agent + 真实通道 + oneshot REST）
// ===========================================================================

#[tokio::test]
async fn full_upgrade_roundtrip_upload_command_drain_download_version_update() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let (addr, handle) = spawn_grpc(&app).await;

    // 1. 上传 1.0.0 包（= Server 版本）。
    let pkg = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
    let up = upload_pkg(&app, &cookie, pkg, "new-binary-bytes").await;
    assert_eq!(up.status(), axum::http::StatusCode::CREATED);
    let sha = body_json(up).await["sha256"].as_str().unwrap().to_string();

    // 2. 建 Agent（0.9.0，窗口内）+ 连上。
    let token = create_agent(&app, "linux-1").await;
    let (mut stream, tx) = connect(addr, &token, v(0, 9, 0)).await;
    recv_handshake(&mut stream).await;
    wait_until(|| async {
        app.state.agents.get_by_name("linux-1").await.unwrap().unwrap().agent_version().unwrap()
            == Some(AgentVersion { major: 0, minor: 9, patch: 0 })
    })
    .await;

    // 3. 单台升级（REST）→ fake Agent 收 UpgradeCommand。
    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/linux-1/upgrade", Some(format!(r#"{{"package_name": "{pkg}"}}"#)), Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    let cmd_msg = recv_cmd(&mut stream).await.expect("应收到 UpgradeCommand");
    let Kind::Upgrade(cmd) = cmd_msg.kind.unwrap() else { panic!("应为 UpgradeCommand") };
    assert_eq!(cmd.package_name, pkg);
    assert_eq!(cmd.sha256, sha);
    assert_eq!(cmd.download_url, download_path(pkg));

    // 4. fake 报 DRAINING → server 落 upgrade_phase=draining + 清 pending。
    tx.send(ChannelMessage {
        kind: Some(Kind::UpgradeStatus(sisyphus_proto::agent::UpgradeStatus {
            phase: sisyphus_proto::agent::UpgradePhase::UpgradeDraining as i32,
            error: String::new(),
        })),
    })
    .await
    .unwrap();
    wait_until(|| async {
        let row = app.state.agents.get_by_name("linux-1").await.unwrap().unwrap();
        row.upgrade_phase.as_deref() == Some("draining") && row.pending_upgrade.is_none()
    })
    .await;

    // 5. 下载校验：fake「下载」即 oneshot 打下载端点（agent token）→ sha256 头匹配。
    let dl = custom_req(&app, "GET", &download_path(pkg), None, None, &[("authorization", format!("Bearer {token}"))], common::DEFAULT_PEER).await;
    assert_eq!(dl.status(), axum::http::StatusCode::OK);
    assert_eq!(dl.headers().get("x-sisyphus-sha256").unwrap().to_str().unwrap(), sha);

    // 6. fake 报 DOWNLOADING/SWAPPING/RESTARTING。
    for phase in [
        sisyphus_proto::agent::UpgradePhase::UpgradeDownloading as i32,
        sisyphus_proto::agent::UpgradePhase::UpgradeSwapping as i32,
        sisyphus_proto::agent::UpgradePhase::UpgradeRestarting as i32,
    ] {
        tx.send(ChannelMessage {
            kind: Some(Kind::UpgradeStatus(sisyphus_proto::agent::UpgradeStatus { phase, error: String::new() })),
        })
        .await
        .unwrap();
    }
    wait_until(|| async {
        app.state.agents.get_by_name("linux-1").await.unwrap().unwrap().upgrade_phase.as_deref() == Some("restarting")
    })
    .await;

    // 7. fake「重启」：断开旧连接，以新版本 1.0.0 重连 → server 清升级态 + 落新版本。
    drop(tx);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut stream2, _tx2) = connect(addr, &token, v(1, 0, 0)).await;
    recv_handshake(&mut stream2).await;
    wait_until(|| async {
        let row = app.state.agents.get_by_name("linux-1").await.unwrap().unwrap();
        row.agent_version().unwrap() == Some(AgentVersion { major: 1, minor: 0, patch: 0 })
            && row.upgrade_phase.is_none()
            && row.pending_upgrade.is_none()
    })
    .await;

    handle.abort();
}

// ===========================================================================
// 过旧 Agent：任务面拒连 + 升级面保留
// ===========================================================================

#[tokio::test]
async fn too_old_agent_task_face_rejected_upgrade_face_preserved() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let (addr, handle) = spawn_grpc(&app).await;

    let pkg = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
    upload_pkg(&app, &cookie, pkg, "bytes").await;

    // 过旧 Agent（0.8.0 < N-1=0.9）连上——握手不拒（只判过新），落版本 + 在线。
    let token = create_agent(&app, "old-1").await;
    let (mut stream, _tx) = connect(addr, &token, v(0, 8, 0)).await;
    recv_handshake(&mut stream).await;
    wait_until(|| async {
        let row = app.state.agents.get_by_name("old-1").await.unwrap().unwrap();
        row.online && row.agent_version().unwrap() == Some(AgentVersion { major: 0, minor: 8, patch: 0 })
    })
    .await;

    // 任务面拒连：在线但 dispatchable=false（version_incompatible）→ match_candidates 排除。
    let server = app.state.agents.server_version();
    let row = app.state.agents.get_by_name("old-1").await.unwrap().unwrap();
    assert!(row.online, "过旧 Agent 仍在线");
    assert!(!row.dispatchable(&server).unwrap(), "过旧 Agent 不可派发（任务面拒连）");
    assert!(app.state.agents.match_candidates(None, &[]).await.unwrap().is_empty(), "过旧 Agent 不进候选");

    // 升级面保留：REST 升级指令仍送达 fake（握手/指令通）。
    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/old-1/upgrade", Some(format!(r#"{{"package_name": "{pkg}"}}"#)), Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "升级面保留：指令应受理");
    let cmd_msg = recv_cmd(&mut stream).await.expect("过旧 Agent 应收到升级指令");
    assert!(matches!(cmd_msg.kind, Some(Kind::Upgrade(_))));

    handle.abort();
}

// ===========================================================================
// 工作区 / 缓存：列表经通道往返 + 删除指令下发
// ===========================================================================

#[tokio::test]
async fn workspace_list_roundtrip_and_clean_delivered() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let (addr, handle) = spawn_grpc(&app).await;

    let token = create_agent(&app, "linux-1").await;
    let (mut stream, tx) = connect(addr, &token, v(1, 0, 0)).await;
    recv_handshake(&mut stream).await;

    // 后台：fake 收 list 请求即回放一条假列表。
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let cmd = recv_cmd(&mut stream).await.expect("收工作区 list 请求");
        match cmd.kind {
            Some(Kind::WorkspaceCmd(c)) if matches!(c.kind, Some(WorkspaceKind::List(_))) => {
                let _ = tx2.send(ChannelMessage {
                    kind: Some(Kind::WorkspaceList(sisyphus_proto::agent::WorkspaceList {
                        entries: vec![sisyphus_proto::agent::WorkspaceEntry {
                            pipeline: "demo".into(),
                            job: "compile".into(),
                            path: "/ws/demo/compile".into(),
                            last_used_at_ms: 1_700_000_000_000,
                        }],
                    })),
                }).await;
            }
            other => panic!("应为 WorkspaceList 请求，得 {other:?}"),
        }
    });

    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/linux-1/workspace/list", None, Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "工作区列表应 200");
    let body = body_json(resp).await;
    assert_eq!(body["entries"][0]["pipeline"], "demo");
    assert_eq!(body["entries"][0]["path"], "/ws/demo/compile");

    // 工作区清理（fire-and-forget）→ 202。
    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/linux-1/workspace/clean", Some(r#"{"pipeline": "demo"}"#.into()), Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    // 从未连接的 Agent → 无会话 → 工作区列表 409（离线）。
    let _offline = create_agent(&app, "offline-1").await;
    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/offline-1/workspace/list", None, Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT, "从未连接的 Agent 应 409");

    handle.abort();
}

#[tokio::test]
async fn cache_list_roundtrip_and_delete_delivered() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let (addr, handle) = spawn_grpc(&app).await;

    let token = create_agent(&app, "linux-1").await;
    let (mut stream, tx) = connect(addr, &token, v(1, 0, 0)).await;
    recv_handshake(&mut stream).await;

    let tx2 = tx.clone();
    tokio::spawn(async move {
        let cmd = recv_cmd(&mut stream).await.expect("收缓存 list 请求");
        match cmd.kind {
            Some(Kind::CacheCmd(c)) if matches!(c.kind, Some(CacheKind::List(_))) => {
                let _ = tx2.send(ChannelMessage {
                    kind: Some(Kind::CacheList(sisyphus_proto::agent::CacheList {
                        entries: vec![sisyphus_proto::agent::CacheEntry {
                            key: "cargo-abc".into(),
                            pipeline: "demo".into(),
                            size_bytes: 5_000,
                            last_used_at_ms: 1_700_000_000_000,
                        }],
                    })),
                }).await;
            }
            other => panic!("应为 CacheList 请求，得 {other:?}"),
        }
    });

    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/linux-1/cache/list", None, Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["entries"][0]["key"], "cargo-abc");
    assert_eq!(body["entries"][0]["size_bytes"], 5_000);

    // 缓存删除（fire-and-forget）→ 202。
    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/linux-1/cache/delete", Some(r#"{"key": "cargo-abc"}"#.into()), Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    handle.abort();
}

/// 全量升级受理摘要：issued/skipped（在线即送 + 已在目标版本跳过）。
#[tokio::test]
async fn upgrade_all_issues_to_non_target_skips_at_target() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let (addr, handle) = spawn_grpc(&app).await;

    let pkg = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
    upload_pkg(&app, &cookie, pkg, "bytes").await;

    // 两个 Agent：a 在 0.9.0（需升级）、b 在 1.0.0（已在目标，跳过）。
    let token_a = create_agent(&app, "a-1").await;
    let (mut sa, _txa) = connect(addr, &token_a, v(0, 9, 0)).await;
    recv_handshake(&mut sa).await;
    let token_b = create_agent(&app, "b-1").await;
    let (mut sb, _txb) = connect(addr, &token_b, v(1, 0, 0)).await;
    recv_handshake(&mut sb).await;
    wait_until(|| async {
        let a = app.state.agents.get_by_name("a-1").await.unwrap().unwrap();
        let b = app.state.agents.get_by_name("b-1").await.unwrap().unwrap();
        a.agent_version().unwrap() == Some(AgentVersion { major: 0, minor: 9, patch: 0 })
            && b.agent_version().unwrap() == Some(AgentVersion { major: 1, minor: 0, patch: 0 })
    })
    .await;

    let resp = req_with_cookie(&app, "POST", "/api/v1/agents/upgrade", Some(format!(r#"{{"package_name": "{pkg}"}}"#)), Some(&cookie)).await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["issued"], 1, "只 a-1（0.9.0）需升级");
    assert_eq!(body["skipped"], 1, "b-1（1.0.0）已在目标跳过");

    handle.abort();
}

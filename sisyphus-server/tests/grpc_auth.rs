//! Agent 通道认证与心跳 proto 缝（票 #47 AC）：真实 tonic 装配 + 真实
//! store，本地 socket 通道闭环。
//!
//! 形态基准：B1 `full_duplex_round_trip_over_inprocess_channel`（proto 缝）
//! —— 起真实 tonic 服务 + 本地 socket，Agent 客户端凭 token 连上后完成
//! 握手 → 心跳 →（停用）踢线全链路。所有用例都落库断言（online /
//! last_seen / system_labels / disk_usage 入库可查），不停靠内部循环细节。
//! 客户端上行流经 mpsc 通道驱动（连上后可继续发心跳/其他帧）。

use sisyphus_proto::agent::{
    ChannelMessage, DiskUsage, Handshake, Heartbeat, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::sched::SchedulerHandle;
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::{api, grpc, store};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

/// 与 workspace 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

/// 当前 Unix 毫秒（与 store 同纪；store 的 now_ms 是 crate 私有，测试自取）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟晚于 Unix 纪元")
        .as_millis() as i64
}

/// 进程内装配：真实 store（临时库）+ 组合根状态。
struct Harness {
    _dir: tempfile::TempDir,
    state: api::AppState,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = api::AppState::new(
        pool,
        dir.path().to_path_buf(),
        false,
        master_key,
        sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES,
    )
    .await
    .expect("装配 AppState");
    Harness { _dir: dir, state }
}

/// 建一个 Agent 条目并返回 (token, 行 id)。
async fn create_agent(state: &api::AppState, name: &str) -> (String, i64) {
    let token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let row = state
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
    (token, row.id)
}

/// 起真实 tonic gRPC 服务（AgentChannel），返回监听地址与 JoinHandle。
async fn spawn_grpc(state: api::AppState) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let sessions = Arc::new(grpc::SessionRegistry::new());
    // 认证/心跳用例不驱动调度循环：丢弃面句柄（任务面帧不转发）。
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

/// 与 Server 建立连接：握手经请求流（mpsc 发送器驱动）发送，返回
/// (响应流, 上行发送器)——连上后可继续发心跳等帧。认证/版本拒绝在
/// `connect` 调用返回 Err（trait 边界：失败即 RPC 错误）。
async fn connect(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    metadata: &[(&'static str, &'static str)],
) -> Result<
    (
        tonic::Streaming<ChannelMessage>,
        mpsc::Sender<ChannelMessage>,
    ),
    tonic::Status,
> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("tcp connect");
    connect_with_channel(channel, token, metadata).await
}

/// 既有通道上的连接（形态共用）。
async fn connect_with_channel(
    channel: Channel,
    token: Option<&str>,
    metadata: &[(&'static str, &'static str)],
) -> Result<
    (
        tonic::Streaming<ChannelMessage>,
        mpsc::Sender<ChannelMessage>,
    ),
    tonic::Status,
> {
    let mut client = AgentChannelClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
    tx.send(ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(version()),
            agent_name: "linux-1".into(),
        })),
    })
    .await
    .expect("send handshake");

    let mut request = tonic::Request::new(ReceiverStream::new(rx));
    if let Some(token) = token {
        request.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}")).expect("值"),
        );
    }
    for (key, value) in metadata {
        request
            .metadata_mut()
            .insert(*key, MetadataValue::try_from(*value).expect("值"));
    }
    let response = client.connect(request).await?;
    Ok((response.into_inner(), tx))
}

/// 从响应流读到握手回执（Server 确认会话建立）。
async fn expect_handshake(stream: &mut tonic::Streaming<ChannelMessage>) {
    let msg = stream.message().await.expect("recv").expect("msg");
    assert!(
        matches!(msg.kind, Some(Kind::Handshake(_))),
        "会话建立应先回握手"
    );
}

/// 等到谓词成立或超时（心跳落库是服务端异步路径，轮询驱动，避免 flaky）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..50 {
        if f().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("条件在 5s 内未成立");
}

#[tokio::test]
async fn rejects_connection_without_token() {
    let h = harness().await;
    let (addr, handle) = spawn_grpc(h.state.clone()).await;

    // 缺 token：握手后认证失败拒连（unauth）。
    let err = connect(addr, None, &[]).await.expect_err("缺 token 应拒连");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert_eq!(
        err.message(),
        "缺 Authorization: Bearer <sisa_ token>",
        "缺头信息明确（前置检查，先于握手）"
    );

    handle.abort();
}

#[tokio::test]
async fn rejects_connection_with_wrong_or_disabled_token() {
    let h = harness().await;
    let (addr, handle) = spawn_grpc(h.state.clone()).await;

    // 错 token（格式对、哈希不存在）：拒连。
    let err = connect(addr, Some("sisa_wrong-token"), &[])
        .await
        .expect_err("错 token 应拒连");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // 建 Agent 拿真 token → 停用 → 同 token 拒连（停用即踢线：下一连接即拒）。
    let (token, id) = create_agent(&h.state, "linux-1").await;
    h.state.agents.set_disabled(id, true).await.expect("停用");
    let err = connect(addr, Some(&token), &[])
        .await
        .expect_err("停用即踢线");
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "停用 Agent 拒连");

    handle.abort();
}

#[tokio::test]
async fn valid_token_establishes_session_and_marks_online() {
    let h = harness().await;
    let (addr, handle) = spawn_grpc(h.state.clone()).await;

    let (token, id) = create_agent(&h.state, "linux-1").await;

    // 连接（握手 + 系统标签 metadata）→ 收握手回执 → 在线落库。
    let (mut stream, tx) = connect(
        addr,
        Some(&token),
        &[("x-sisyphus-os", "linux"), ("x-sisyphus-arch", "amd64")],
    )
    .await
    .expect("有效 token 应连上");
    expect_handshake(&mut stream).await;

    let row = h.state.agents.get(id).await.expect("查").expect("应存在");
    assert!(row.online, "连接即上线");
    assert!(row.last_seen_at.is_some(), "上线刷 last_seen");
    assert!(
        row.system_labels.contains("sisyphus/os=linux"),
        "系统标签随连接上报：{}",
        row.system_labels
    );
    assert!(
        row.system_labels.contains("sisyphus/arch=amd64"),
        "arch 随连接上报"
    );
    assert!(
        !row.system_labels.contains("container"),
        "未上报的容器事实不置"
    );

    // 发送心跳（带磁盘占用）→ 落库可查。
    tx.send(ChannelMessage {
        kind: Some(Kind::Heartbeat(Heartbeat {
            disk: Some(DiskUsage {
                volumes: vec![sisyphus_proto::agent::VolumeUsage {
                    mount_point: "/".into(),
                    total_bytes: 100,
                    free_bytes: 40,
                }],
                cache_bytes: 5,
                workspace_bytes: 10,
            }),
        })),
    })
    .await
    .expect("send heartbeat");

    wait_until(|| async {
        let row = h.state.agents.get(id).await.expect("查").expect("应存在");
        match row.disk_usage().expect("解析磁盘占用") {
            Some(disk) => {
                assert_eq!(disk.cache_bytes, 5);
                assert_eq!(disk.workspace_bytes, 10);
                assert_eq!(disk.volumes.len(), 1);
                assert_eq!(disk.volumes[0].mount_point, "/");
                assert_eq!(disk.volumes[0].free_bytes, 40);
                true
            }
            None => false,
        }
    })
    .await;

    // 会话期间停用：下一帧踢线（任何帧都复核 token 仍有效——停用心跳
    // 不生效 → 会话断开 → 对端流 EOF）。
    h.state.agents.set_disabled(id, true).await.expect("停用");
    tx.send(ChannelMessage {
        kind: Some(Kind::Heartbeat(Heartbeat { disk: None })),
    })
    .await
    .expect("send");
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return true,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "停用后会话应断开");
    assert!(closed.expect("超时已处理"), "停用即踢线：流结束");

    // 行仍在（可管理）：停用落定、在线标记留给 45s 扫描兜底。
    let row = h.state.agents.get(id).await.expect("查").expect("应存在");
    assert!(row.disabled, "停用落定");

    handle.abort();
}

#[tokio::test]
async fn heartbeat_sweep_marks_stale_agents_offline() {
    // 在线判定语义（ADR-0007/0008：45s 无心跳判离线）的驱动面：把 Agent
    // 的 last_seen 拨到 45s 之前，跑一轮扫描即判离线（不依赖真实时钟）。
    let h = harness().await;
    let (_token, id) = create_agent(&h.state, "linux-1").await;
    let stale_at = now_ms() - (grpc::HEARTBEAT_TIMEOUT_MS + 1_000);
    h.state
        .agents
        .mark_online(id, "[]", None, stale_at)
        .await
        .expect("置在线（过期 last_seen）");
    assert!(
        h.state
            .agents
            .get(id)
            .await
            .expect("查")
            .expect("应存在")
            .online
    );

    grpc::heartbeat_sweep_once(&h.state).await;

    let row = h.state.agents.get(id).await.expect("查").expect("应存在");
    assert!(!row.online, "45s 无心跳判离线");
    assert!(
        row.last_seen_at.expect("离线也刷 last_seen") >= stale_at,
        "离线刷新 last_seen 到扫描时刻"
    );
}

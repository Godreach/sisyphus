//! 同进程双服务冒烟（票 B2a-T3 AC）：REST 与 gRPC 各自监听互不干扰。
//!
//! 本测试的对象是「监听」本身，故走真实 socket；端点行为测试在
//! rest_api.rs 经 oneshot 进程内完成。gRPC 侧复用 B1 的真实握手闭环，
//! 守住「REST 接入不破 Agent 通道」。

use sisyphus_proto::agent::{
    ChannelMessage, Handshake, Version, agent_channel_client::AgentChannelClient,
    channel_message::Kind,
};
use sisyphus_server::{api, grpc, store};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn rest_and_grpc_serve_side_by_side() {
    // 与二进制 main 相同的装配：REST 走 api::router(AppState, web 覆盖目录)
    // （池经 store::bootstrap），gRPC 走 grpc::service()。
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
    let web_override_dir = dir.path().join(sisyphus_server::config::WEB_DIR);

    // 建一个 Agent 条目（票 #47 起通道认证 Bearer sisa_ token）：握手
    // 冒烟带真凭据过认证面。
    let token = sisyphus_server::auth::generate_token(sisyphus_server::auth::TokenFamily::Agent);
    let code = sisyphus_server::auth::generate_register_code();
    state
        .agents
        .create(sisyphus_server::store::agents::NewAgent {
            name: "smoke-agent".into(),
            token_hash: sisyphus_server::auth::token_hash(&token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: sisyphus_server::auth::token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建 Agent 条目");

    let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind REST");
    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gRPC");
    let rest_addr = rest_listener.local_addr().expect("REST addr");
    let grpc_addr = grpc_listener.local_addr().expect("gRPC addr");

    let grpc_state = state.clone();
    let check_state = state.clone();
    let sessions = Arc::new(grpc::SessionRegistry::new());
    let scheduler = sisyphus_server::sched::SchedulerHandle::discard();
    let rest = tokio::spawn(async move {
        axum::serve(rest_listener, api::router(state, web_override_dir))
            .await
            .expect("REST serve")
    });
    let grpc = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc::service(grpc_state, sessions, scheduler))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await
            .expect("gRPC serve")
    });

    // REST：healthz 经真实 socket 返回 200。
    assert_eq!(http1_get_status(rest_addr, "/healthz").await, 200);

    // gRPC：Agent 握手经真实通道收到 Server 版本回执（带 Bearer token 过
    // 通道认证面，票 #47）。
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{grpc_addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentChannelClient::new(channel);
    let handshake = ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            // Server 为 workspace 版本 1.0.0，同版本在兼容窗口内。
            agent_version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
            }),
            agent_name: "smoke-agent".into(),
        })),
    };
    let mut request = tonic::Request::new(tokio_stream::iter(vec![handshake]));
    request.metadata_mut().insert(
        "authorization",
        tonic::metadata::MetadataValue::try_from(format!("Bearer {token}")).expect("值"),
    );
    let outbound = client.connect(request).await.expect("connect call");
    let first = outbound
        .into_inner()
        .message()
        .await
        .expect("recv")
        .expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))));

    // 握手后会话任务会置 Agent 在线（通道认证面全链路：连接即上线）。
    let row = check_state
        .agents
        .get_by_name("smoke-agent")
        .await
        .expect("查")
        .expect("应存在");
    assert!(row.online, "通道认证通过即置在线");

    rest.abort();
    grpc.abort();
}

/// 裸 HTTP/1.1 GET：只解析状态行——探活级冒烟，不为它引 HTTP 客户端依赖。
async fn http1_get_status(addr: std::net::SocketAddr, path: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let head = String::from_utf8_lossy(&buf);
    head.lines()
        .next()
        .expect("状态行")
        .split_whitespace()
        .nth(1)
        .expect("状态码")
        .parse()
        .expect("u16")
}

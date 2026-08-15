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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn rest_and_grpc_serve_side_by_side() {
    // 与二进制 main 相同的装配：REST 走 api::router(AppState)（池经
    // store::bootstrap），gRPC 走 grpc::service()。
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let state = api::AppState::new(pool);

    let rest_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind REST");
    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gRPC");
    let rest_addr = rest_listener.local_addr().expect("REST addr");
    let grpc_addr = grpc_listener.local_addr().expect("gRPC addr");

    let rest = tokio::spawn(async move {
        axum::serve(rest_listener, api::router(state))
            .await
            .expect("REST serve")
    });
    let grpc = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc::service())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                grpc_listener,
            ))
            .await
            .expect("gRPC serve")
    });

    // REST：healthz 经真实 socket 返回 200。
    assert_eq!(http1_get_status(rest_addr, "/healthz").await, 200);

    // gRPC：Agent 握手经真实通道收到 Server 版本回执。
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
    let outbound = client
        .connect(tonic::Request::new(tokio_stream::iter(vec![handshake])))
        .await
        .expect("connect call");
    let first = outbound
        .into_inner()
        .message()
        .await
        .expect("recv")
        .expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))));

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

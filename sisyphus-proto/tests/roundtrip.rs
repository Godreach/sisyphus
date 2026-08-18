//! proto 契约 round-trip 冒烟（Spec B1 T2）。
//!
//! 验证契约可编译、可用：握手、任务规格下发与回执能真实往返。
//! B1 不实现真实注册码签发（归后续批次）——这里用最小 stub 证明
//! 契约生成的服务与消息在真实 tonic 装配下工作。

use prost::Message;
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, JobSpec, Version,
    agent_channel_client::AgentChannelClient,
    agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

/// Server 侧最小实现：收握手后回一个握手确认、再下发一个任务规格。
struct StubServer;

#[tonic::async_trait]
impl AgentChannel for StubServer {
    type ConnectStream = ReceiverStream<Result<ChannelMessage, Status>>;

    async fn connect(
        &self,
        mut request: Request<Streaming<ChannelMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let inbound = request.get_mut();
        let mut received_job: Option<String> = None;
        while let Some(msg) = inbound
            .message()
            .await
            .map_err(|e| Status::internal(format!("read inbound: {e}")))?
        {
            match msg.kind {
                Some(Kind::Handshake(h)) => {
                    assert_eq!(h.agent_version.unwrap().major, 1);
                }
                Some(Kind::JobAck(ack)) => {
                    assert!(ack.accepted);
                    received_job = Some(ack.job_id);
                }
                _ => {}
            }
        }

        // 回发：握手确认 + 一个任务规格（证明 Server -> Agent 方向可用）。
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let ack = ChannelMessage {
            kind: Some(Kind::JobSpec(Box::new(JobSpec {
                job_id: "job-1".into(),
                pipeline_name: "demo".into(),
                job_name: "build".into(),
                build_number: 1,
                attempt: 0,
                log_limit_bytes: 0,
                steps: vec![],
                env: Default::default(),
                exec_env: None,
                timeout_minutes: 0,
                uploads: vec![],
                downloads: vec![],
                caches: vec![],
                secrets: vec![],
                scm_credential: None,
                labels: vec![],
                retry_count: 0,
                allow_failure: false,
            }))),
        };
        tx.try_send(Ok(ack)).expect("send spec");
        drop(tx);

        assert_eq!(received_job.as_deref(), Some("job-1"));
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[tokio::test]
async fn version_message_round_trips() {
    let v = Version {
        major: 1,
        minor: 0,
        patch: 0,
    };
    let mut buf = Vec::new();
    v.encode(&mut buf).expect("encode");
    let decoded = Version::decode(buf.as_slice()).expect("decode");
    assert_eq!(v, decoded);
}

#[tokio::test]
async fn handshake_carries_version() {
    let hs = Handshake {
        agent_version: Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
        }),
        agent_name: "builder-01".into(),
    };
    let mut buf = Vec::new();
    hs.encode(&mut buf).expect("encode");
    let decoded = Handshake::decode(buf.as_slice()).expect("decode");
    assert_eq!(decoded.agent_version.unwrap().major, 1);
    assert_eq!(decoded.agent_name, "builder-01");
}

#[tokio::test]
async fn full_duplex_round_trip_over_inprocess_channel() {
    // 起一个真实 tonic 服务 + 本地 socket 通道，Agent 客户端连上后
    // 完成：发握手 + 任务回执（Agent -> Server），收任务规格（Server -> Agent）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");

    let addr = listener.local_addr().expect("addr");

    let serve = async {
        tonic::transport::Server::builder()
            .add_service(AgentChannelServer::new(StubServer))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    };
    let server_task = tokio::spawn(serve);

    // 客户端连接
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentChannelClient::new(channel);

    // Agent 上行：握手 + 任务回执
    let handshake = ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
            }),
            agent_name: "builder-01".into(),
        })),
    };
    let ack = ChannelMessage {
        kind: Some(Kind::JobAck(sisyphus_proto::agent::JobAck {
            job_id: "job-1".into(),
            accepted: true,
            error: String::new(),
        })),
    };

    // bidi 流：客户端请求流由 tokio_stream 迭代器构造（消息本体，非 Result）。
    let inbound = tokio_stream::iter(vec![handshake, ack]);
    let outbound = client
        .connect(Request::new(inbound))
        .await
        .expect("call connect");

    let mut responses = outbound.into_inner();
    let first = responses.message().await.expect("recv").expect("msg");
    // Server 回发任务规格
    assert!(matches!(first.kind, Some(Kind::JobSpec(_))));

    server_task.abort();
}

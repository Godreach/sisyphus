//! Server 的 Agent 通道（gRPC，ADR-0007）。
//!
//! B1 只做最小握手闭环：Server 收到 Agent 握手，校验版本并回发自己的版本。
//! 真实任务下发/日志/取消随后续批次。

use sisyphus_proto::agent::{
    agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind,
    ChannelMessage, Handshake, Version,
};
use sisyphus_proto::version;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

/// Server 侧版本（ADR-0010：与 Agent 同版本成对发布）。
pub fn server_version() -> Version {
    version::VERSION
}

/// 最小握手实现：Agent 过新（> Server）拒连；过旧/窗口内记录日志。
#[derive(Default)]
pub struct AgentChannelService;

#[tonic::async_trait]
impl AgentChannel for AgentChannelService {
    type ConnectStream = ReceiverStream<Result<ChannelMessage, Status>>;

    async fn connect(
        &self,
        mut request: Request<Streaming<ChannelMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let inbound = request.get_mut();
        let mut agent_version: Option<Version> = None;

        while let Some(msg) = inbound
            .message()
            .await
            .map_err(|e| Status::internal(format!("read inbound: {e}")))?
        {
            if let Some(Kind::Handshake(h)) = msg.kind {
                agent_version = h.agent_version;
                break;
            }
        }

        let agent_version = agent_version.ok_or_else(|| {
            Status::invalid_argument("首帧必须是握手（含 Agent 版本号）")
        })?;

        // 版本窗口（ADR-0010/0017）：Agent 过新直接拒连。
        if version::peer_too_new(&agent_version, &server_version()) {
            return Err(Status::failed_precondition(format!(
                "Agent 版本 {}.{}.{} 过新，拒绝连接（Server 为 {}.{}.{}）",
                agent_version.major,
                agent_version.minor,
                agent_version.patch,
                server_version().major,
                server_version().minor,
                server_version().patch,
            )));
        }

        // 回发 Server 版本（Agent 侧据此确认窗口）。
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let reply = ChannelMessage {
            kind: Some(Kind::Handshake(Handshake {
                agent_version: Some(server_version()),
                agent_name: "sisyphus-server".into(),
            })),
        };
        tx.try_send(Ok(reply)).expect("send handshake reply");
        drop(tx);

        tracing::info!(
            "agent connected: v{}.{}.{}",
            agent_version.major,
            agent_version.minor,
            agent_version.patch,
        );
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// 把 AgentChannel 服务挂到 tonic 路由上。
pub fn service() -> AgentChannelServer<AgentChannelService> {
    AgentChannelServer::new(AgentChannelService)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::version;

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn same_version_in_window() {
        assert!(version::compatible(&v(1, 0, 0), &server_version()));
    }

    #[test]
    fn older_agent_in_window() {
        // N-1 兼容窗口（ADR-0010）：旧 Agent 可连（任务面细化归后续）。
        assert!(version::compatible(&v(0, 9, 0), &server_version()));
    }

    #[test]
    fn newer_agent_rejected() {
        assert!(!version::compatible(&v(2, 0, 0), &server_version()));
        assert!(version::peer_too_new(&v(1, 1, 0), &server_version()));
    }
}

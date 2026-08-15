//! Agent 通道（gRPC 连接管理，ADR-0007）。
//!
//! B1 只做最小握手闭环：Agent 主动外连 Server，发握手（含自身版本），
//! 收 Server 版本回发，确认版本窗口。真实心跳/任务/日志随后续批次。

use sisyphus_proto::agent::{
    agent_channel_client::AgentChannelClient,
    channel_message::Kind, ChannelMessage, Handshake, Version,
};
use sisyphus_proto::version;
use tonic::transport::Channel;

/// Agent 侧版本（ADR-0010：与 Server 同版本成对发布）。
pub fn agent_version() -> Version {
    version::VERSION
}

/// 连接 Server 并完成握手。返回 `(client, 对端版本)`。
pub async fn connect_and_handshake(
    server_url: &str,
    agent_name: &str,
) -> Result<(AgentChannelClient<Channel>, Version), String> {
    let channel = tonic::transport::Endpoint::from_shared(server_url.to_string())
        .map_err(|e| format!("无效 server-url {server_url}: {e}"))?
        .connect()
        .await
        .map_err(|e| format!("连接 {server_url} 失败: {e}"))?;

    let mut client = AgentChannelClient::new(channel);

    let handshake = ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(agent_version()),
            agent_name: agent_name.to_string(),
        })),
    };

    let inbound = tokio_stream::iter(vec![handshake]);
    let response = client
        .connect(tonic::Request::new(inbound))
        .await
        .map_err(|e| format!("握手请求失败: {e}"))?;

    let mut stream = response.into_inner();
    let mut server_version = None;
    while let Some(msg) = stream
        .message()
        .await
        .map_err(|e| format!("读握手回包失败: {e}"))?
    {
        if let Some(Kind::Handshake(h)) = msg.kind {
            server_version = h.agent_version;
            break;
        }
    }

    let server_version = server_version.ok_or_else(|| "Server 未回发版本".to_string())?;

    // 版本窗口（ADR-0010/0017）：Server 过新则本地明确报错。
    if version::peer_too_new(&server_version, &agent_version()) {
        return Err(format!(
            "Server 版本 {}.{}.{} 过新，Agent 拒连（本地为 {}.{}.{}）",
            server_version.major,
            server_version.minor,
            server_version.patch,
            agent_version().major,
            agent_version().minor,
            agent_version().patch,
        ));
    }

    Ok((client, server_version))
}

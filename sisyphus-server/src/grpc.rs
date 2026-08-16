//! Server 的 Agent 通道（gRPC，ADR-0007；票 B2c-T3 接线）。
//!
//! - **通道认证**：握手（版本窗口，B1 语义保留）后校验 Agent token——
//!   `Authorization: Bearer sisa_…`，SHA-256 查 agents 表 + 未停用
//!   （[`crate::store::agents::AgentRepo::find_active_by_hash`]），失败拒连
//!   （`unauthenticated`）。token 与系统标签走 gRPC metadata：proto 演进
//!   只加字段、本批不动契约，而 token 是连接级凭据、os/arch/container 是
//!   连接级事实（探测一次、随连接呈送），都天然属于请求元数据面。
//! - **停用即踢线**：会话内每帧复核 token 仍有效——停用/吊销的 Agent
//!   下一帧（下一请求）即断开，不等下一次连接才受拒。
//! - **心跳与在线判定**（ADR-0007/0008）：Agent 15s 一报心跳，45s 无心跳
//!   判离线（[`heartbeat_sweep`] 后台扫描 + 断连即离线）。心跳刷新
//!   online/last_seen、整组替换系统标签（连接面事实）、落磁盘占用
//!   （ADR-0019：卷级/缓存/工作区采样）。
//! - **任务面**（JobSpec 下发/回执/状态/在途上报/取消）随 sched 批次
//!   （B2c-T4）接线；本批收到非握手/心跳帧一律忽略（契约演进只加字段、
//!   unknown 帧前瞻兼容），仅保留「复核 token 仍有效」的踢线语义。

use std::time::Duration;

use sisyphus_proto::agent::{
    agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind,
    ChannelMessage, DiskUsage, Handshake, Version,
};
use sisyphus_proto::version;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming, metadata::MetadataMap};

use crate::api::AppState;
use crate::auth::token_hash;
use crate::store::agents::{AgentDiskUsage, VolumeUsage};
use crate::store::now_ms;

/// 心跳间隔语义（ADR-0007）：Agent 15s 一报。
pub const HEARTBEAT_INTERVAL_MS: i64 = 15_000;
/// 在线判定（ADR-0007/0008）：45s 无心跳判离线。
pub const HEARTBEAT_TIMEOUT_MS: i64 = 45_000;
/// 离线扫描周期（秒级整秒，sleep 用）：与心跳间隔同量级——Agent 掉线后
/// 最迟 15s+45s 判离线（落在「45s 无心跳判离线」的观测窗口内）。
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// 通道认证凭据头（Bearer 语义与 REST PAT 面同形）。
const META_AUTHORIZATION: &str = "authorization";
/// 系统标签 metadata 头名（Agent 连接面事实：os/arch/container 探测结果；
/// 取值域见 ADR-0008，不可手编）。
const META_OS: &str = "x-sisyphus-os";
const META_ARCH: &str = "x-sisyphus-arch";
const META_CONTAINER: &str = "x-sisyphus-container";

/// Server 侧版本（ADR-0010：与 Agent 同版本成对发布）。
pub fn server_version() -> Version {
    version::VERSION
}

/// Agent 通道服务：持有组合根状态（认证面 + 心跳面共用 repo，与 REST
/// 同装配）。
pub struct AgentChannelService {
    state: AppState,
}

impl AgentChannelService {
    /// 以组合根状态构造。
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl AgentChannel for AgentChannelService {
    type ConnectStream = ReceiverStream<Result<ChannelMessage, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<ChannelMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        // 凭据与系统标签先取（metadata 与流分属 Request 不同字段，取完
        // 再拿流的所有权）。
        let token = bearer_token(request.metadata())?;
        let token_hash = token_hash(&token);
        let labels_json = system_labels_from_metadata(request.metadata());

        let mut inbound = request.into_inner();

        // 首帧必须是握手（含 Agent 版本号；B1 语义保留）。
        let mut agent_version = None;
        while let Some(msg) = read_inbound(&mut inbound).await? {
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

        // 通道认证：token 哈希查 agents 表 + 未停用（停用即踢线：认证面
        // 不命中一律拒连，与「行不存在」不可区分）。查库失败按服务端错误
        // 拒连（不带病放行）。
        let agent = self
            .state
            .agents
            .find_active_by_hash(&token_hash)
            .await
            .map_err(|e| Status::internal(format!("认证查库失败：{e}")))?
            .ok_or_else(|| Status::unauthenticated("Agent token 无效或已停用"))?;

        // 上线：置在线、刷 last_seen、整组替换系统标签（连接面事实随
        // 每次连接重写；max_concurrency 保持管理员设定，本批无建议并发）。
        self.state
            .agents
            .mark_online(agent.id, &labels_json, None, now_ms())
            .await
            .map_err(|e| Status::internal(format!("上线落库失败：{e}")))?;

        tracing::info!(agent = %agent.name, "agent connected（通道认证通过）");

        let (tx, rx) = mpsc::channel(16);
        // 会话任务：回发握手确认 → 逐帧处理（心跳落库 + 停用踢线复核）；
        // 对端关流/断开或停用即退出（下线由 45s 扫描兜底，通道断开不抢跑）。
        tokio::spawn(session_loop(
            self.state.clone(),
            agent.name.clone(),
            agent.id,
            token_hash,
            labels_json,
            ChannelMessage {
                kind: Some(Kind::Handshake(Handshake {
                    agent_version: Some(server_version()),
                    agent_name: "sisyphus-server".into(),
                })),
            },
            inbound,
            tx,
        ));

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// 会话循环（连接生命周期）：回发握手确认，此后逐帧处理。心跳帧落
/// 在线/标签/磁盘占用（停用不生效即断开）；其余帧只复核 token 仍有效
/// （停用/吊销的 Agent 下一请求即断开）。任务面帧的消费随 sched 批次。
#[allow(clippy::too_many_arguments)]
async fn session_loop(
    state: AppState,
    agent: String,
    agent_id: i64,
    token_hash: String,
    labels_json: String,
    handshake_reply: ChannelMessage,
    mut inbound: Streaming<ChannelMessage>,
    tx: mpsc::Sender<Result<ChannelMessage, Status>>,
) {
    if tx.send(Ok(handshake_reply)).await.is_err() {
        return; // 对端已断开，会话无意义
    }

    loop {
        let msg = match read_inbound(&mut inbound).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break, // 对端关流
            Err(_) => break,   // 读帧失败（连接被重置等）：下线归 45s 扫描
        };

        match msg.kind {
            Some(Kind::Heartbeat(heartbeat)) => {
                // 心跳面：disabled 的 Agent 心跳不生效（返回 false）——
                // 停用即踢线，不等下一次连接受拒。
                let disk_json = heartbeat.disk.map(disk_usage_json);
                let ok = match state
                    .agents
                    .heartbeat(agent_id, &labels_json, disk_json.as_deref(), now_ms())
                    .await
                {
                    Ok(ok) => ok,
                    Err(e) => {
                        tracing::warn!(agent = %agent, error = %e, "心跳落库失败");
                        continue;
                    }
                };
                if !ok {
                    tracing::info!(agent = %agent, "agent 已停用/吊销：断开会话");
                    break;
                }
            }
            _ => {
                // 任务面消息（回执/状态/在途上报）随 sched 批次消费；本批
                // 保留「下一请求即拒」的踢线复核。查库失败（瞬态 IO）不断
                // 健康会话——只有「明确查到且已停用/不存在」才踢线。
                match state.agents.find_active_by_hash(&token_hash).await {
                    Ok(None) => {
                        tracing::info!(agent = %agent, "agent 已停用/吊销：断开会话");
                        break;
                    }
                    Ok(Some(_)) => {}
                    Err(e) => {
                        tracing::warn!(agent = %agent, error = %e, "踢线复核查库失败，维持会话");
                    }
                }
            }
        }
    }
}

/// 进程内心跳超时扫描（ADR-0007/0008）：45s 无心跳判离线。由启动路径
/// spawn（与 gRPC 服务同生命周期）；周期 15s，Agent 掉线后最迟 15s+45s
/// 判离线并置 online=0（sched 批次据此不接新任务）。
pub async fn heartbeat_sweep(state: AppState) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        heartbeat_sweep_once(&state).await;
    }
}

/// 一轮超时扫描（`heartbeat_sweep` 的载荷；独立成函数供测试直接驱动，
/// 不依赖真实时钟——proto 缝用例拨 old last_seen 后跑一轮即断言）。
pub async fn heartbeat_sweep_once(state: &AppState) {
    let now = now_ms();
    let online = match state.agents.list_online().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "心跳超时扫描失败");
            return;
        }
    };
    for agent in online {
        let last_seen = agent.last_seen_at.unwrap_or(0);
        if now - last_seen >= HEARTBEAT_TIMEOUT_MS {
            if let Err(e) = state.agents.mark_offline(agent.id, now).await {
                tracing::warn!(agent = %agent.name, error = %e, "离线落库失败");
            } else {
                tracing::info!(agent = %agent.name, "agent 心跳超时判离线");
            }
        }
    }
}

/// 读一帧上行消息（流读完返回 `None`）。
async fn read_inbound(
    inbound: &mut Streaming<ChannelMessage>,
) -> Result<Option<ChannelMessage>, Status> {
    inbound
        .message()
        .await
        .map_err(|e| Status::internal(format!("read inbound: {e}")))
}

/// 取通道凭据：`Authorization: Bearer <sisa_ token>`（Bearer 语义与 REST
/// PAT 面同形；缺头/非 Bearer/空值一律 `unauthenticated`）。
fn bearer_token(metadata: &MetadataMap) -> Result<String, Status> {
    let value = metadata
        .get(META_AUTHORIZATION)
        .ok_or_else(|| Status::unauthenticated("缺 Authorization: Bearer <sisa_ token>"))?
        .to_str()
        .map_err(|_| Status::unauthenticated("Authorization 头不是合法文本"))?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .ok_or_else(|| Status::unauthenticated("Authorization 须为 Bearer 形态"))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(Status::unauthenticated("Bearer 令牌为空"));
    }
    Ok(token.to_string())
}

/// 系统标签（连接面事实）：`x-sisyphus-os/arch/container` metadata → JSON
/// 数组（`sisyphus/key=value`）。缺省/空值不置（无该事实）。
fn system_labels_from_metadata(metadata: &MetadataMap) -> String {
    let mut labels = Vec::new();
    for (header, key) in [
        (META_OS, "sisyphus/os"),
        (META_ARCH, "sisyphus/arch"),
        (META_CONTAINER, "sisyphus/container"),
    ] {
        if let Some(value) = metadata
            .get(header)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.trim().is_empty())
        {
            labels.push(format!("{key}={}", value.trim()));
        }
    }
    serde_json::to_string(&labels).expect("系统标签 JSON 序列化恒可成功（纯字符串）")
}

/// proto `DiskUsage` → 落库形态 JSON（store 不依赖 proto，转换收在调用侧）。
fn disk_usage_json(disk: DiskUsage) -> String {
    let usage = AgentDiskUsage {
        volumes: disk
            .volumes
            .into_iter()
            .map(|v| VolumeUsage {
                mount_point: v.mount_point,
                total_bytes: v.total_bytes,
                free_bytes: v.free_bytes,
            })
            .collect(),
        cache_bytes: disk.cache_bytes,
        workspace_bytes: disk.workspace_bytes,
    };
    serde_json::to_string(&usage).expect("磁盘占用 JSON 序列化恒可成功（纯 i64/string）")
}

/// 把 AgentChannel 服务挂到 tonic 路由上（注入组合根状态）。
pub fn service(state: AppState) -> AgentChannelServer<AgentChannelService> {
    AgentChannelServer::new(AgentChannelService::new(state))
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

    #[test]
    fn bearer_token_parses_bearer_form_and_rejects_others() {
        let mut meta = tonic::metadata::MetadataMap::new();
        assert!(bearer_token(&meta).is_err(), "缺头应拒");

        meta.insert(META_AUTHORIZATION, "sisa_abc".parse().expect("值"));
        let err = bearer_token(&meta).expect_err("非 Bearer 形态应拒");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        meta.insert(META_AUTHORIZATION, "Bearer sisa_abc".parse().expect("值"));
        assert_eq!(bearer_token(&meta).expect("Bearer"), "sisa_abc");

        meta.insert(META_AUTHORIZATION, "bearer sisa_abc".parse().expect("值"));
        assert_eq!(bearer_token(&meta).expect("小写 bearer"), "sisa_abc");

        meta.insert(META_AUTHORIZATION, "Bearer   ".parse().expect("值"));
        assert!(bearer_token(&meta).is_err(), "空令牌应拒");
    }

    #[test]
    fn system_labels_map_metadata_to_json_array() {
        let mut meta = tonic::metadata::MetadataMap::new();
        assert_eq!(system_labels_from_metadata(&meta), "[]", "缺省无事实");

        meta.insert(META_OS, "linux".parse().expect("值"));
        meta.insert(META_ARCH, "amd64".parse().expect("值"));
        meta.insert(META_CONTAINER, "".parse().expect("值")); // 空值不置
        assert_eq!(
            system_labels_from_metadata(&meta),
            r#"["sisyphus/os=linux","sisyphus/arch=amd64"]"#,
            "仅非空事实入列"
        );
    }

    #[test]
    fn disk_usage_converts_to_json_shape() {
        let disk = DiskUsage {
            volumes: vec![sisyphus_proto::agent::VolumeUsage {
                mount_point: "/".into(),
                total_bytes: 100,
                free_bytes: 40,
            }],
            cache_bytes: 5,
            workspace_bytes: 10,
        };
        assert_eq!(
            disk_usage_json(disk),
            r#"{"volumes":[{"mount_point":"/","total_bytes":100,"free_bytes":40}],"cache_bytes":5,"workspace_bytes":10}"#,
            "与落库形态（AgentDiskUsage）同构"
        );
    }
}

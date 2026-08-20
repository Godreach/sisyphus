//! Server 的 Agent 通道（gRPC，ADR-0007；票 B2c-T3 通道认证 + B2c-T4 任务面）。
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
//! - **任务面**（票 B2c-T4，ADR-0008）：JobSpec 下发（[`JobDispatcher`] 的
//!   生产实现 [`GrpcDispatcher`]——把调度匹配结果经本通道真实下发）、
//!   JobAck（槽位占用/释放）、JobStatus（running/unknown/终态）、
//!   JobReported（重连重建 + 补发挂起取消）、CancelBuild（build 级取消 /
//!   fail-fast 级联）。上行任务面帧转发给调度循环（[`SchedulerHandle`]），
//!   调度侧落库 + engine 推进。
//! - **在线判定事件**：上线/离线发布 [`Event::AgentOnline`]/[`Event::AgentOffline`]
//!   （sched 据此转 unknown/匹配重算；UI 在线态）。
//! - **日志面**（票 #73，ADR-0013）：`Kind::LogBatch` 落库
//!   （[`handle_log_batch`]——按 start_seq 幂等，断线补传不重不乱序）+
//!   事件总线广播（SSE 尾随热通知）。
//! - **会话注册表**（[`SessionRegistry`]）：agent_id → 会话发送器，随连接
//!   建立/断开维护——JobSpec/CancelBuild 的下发目的地。trait 缝隔离：
//!   sched 只依赖 `JobDispatcher`，不依赖 tonic。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sisyphus_proto::agent::{
    CancelBuild, ChannelMessage, DiskUsage, Handshake, UpgradeCommand, Version,
    agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind,
};
use sisyphus_proto::version;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming, metadata::MetadataMap};

use crate::api::AppState;
use crate::auth::token_hash;
use crate::engine::{ResolvedJobSpec, ResolvedStep, Vcs};
use crate::events::Event;
use crate::sched::{JobDispatcher, SchedError, SchedulerHandle};
use crate::store::LogStore;
use crate::store::agents::{AgentDiskUsage, AgentVersion, VolumeUsage};
use crate::store::jobs::JobRow;
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

/// 每会话下行发送通道容量（调度下发 burst：阶段并行任务同时入池）。缓冲
/// 满即背压——session_loop 等待，不丢下行帧。
const SESSION_TX_CAPACITY: usize = 64;

/// REST「经通道查询」请求-响应往返的最长等待（工作区/缓存列表：发指令 →
/// 等上行响应帧）。超时即 504——UI 可重发查询（ADR-0011/0012 列表响应无
/// 离线补发，丢了无害）。
const AWAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Server 侧版本（ADR-0010：与 Agent 同版本成对发布）。
pub fn server_version() -> Version {
    version::VERSION
}

/// 经通道往返的响应种类（工作区列表 / 缓存列表）。键入 `(agent_id, kind)`：
/// 每 Agent 每种响应至多一个待满足请求（UI 一次查一种），免并发往返竞态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AwaitKind {
    /// 工作区列表响应（`Kind::WorkspaceList`）。
    WorkspaceList,
    /// 缓存列表响应（`Kind::CacheList`）。
    CacheList,
}

/// 经通道往返的错误（REST 映射 409/504）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitError {
    /// Agent 离线（无会话 / 会话断开）——REST 409。
    Offline,
    /// 响应超时（Agent 在线但未在 [`AWAIT_TIMEOUT`] 内回帧）——REST 504。
    Timeout,
}

/// 在线 Agent 会话注册表：agent_id → 会话下行发送器。JobSpec/CancelBuild/
/// UpgradeCommand 的下发目的地（[`GrpcDispatcher`]）+ REST「经通道查询」的
/// 请求-响应往返（[`Self::send_and_await`]）。连接建立注册、断开/踢线注销。
#[derive(Default)]
pub struct SessionRegistry {
    /// 下行发送器（指令下发目的地）。
    inner: RwLock<HashMap<i64, mpsc::Sender<Result<ChannelMessage, Status>>>>,
    /// 待满足的往返响应（REST 工作区/缓存列表查询等上行帧）：键 `(agent_id,
    /// kind)` → 回应 oneshot。session_loop 收到 `WorkspaceList`/`CacheList`
    /// 经 [`Self::fulfill`] 满足；断开/超时经 [`Self::cancel`] 丢弃。
    pending: RwLock<HashMap<(i64, AwaitKind), oneshot::Sender<ChannelMessage>>>,
}

impl std::fmt::Debug for SessionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRegistry")
            .finish_non_exhaustive()
    }
}

impl SessionRegistry {
    /// 新建注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册会话（连接建立后）。重复注册（同 Agent 并发连接）覆盖旧会话——
    /// 最晚连接优先（Agent 重连即旧会话作废）。
    async fn register(&self, agent_id: i64, tx: mpsc::Sender<Result<ChannelMessage, Status>>) {
        self.inner.write().await.insert(agent_id, tx);
    }

    /// 注销会话（断开/踢线）：移除下行发送器 + 取消该 Agent 的待满足往返
    /// （oneshot 发送器随移除而 drop → 等待方收 `RecvError` 即 [`AwaitError::Offline`]）。
    async fn unregister(&self, agent_id: i64) {
        self.inner.write().await.remove(&agent_id);
        let mut pending = self.pending.write().await;
        pending.retain(|(id, _), _| *id != agent_id);
    }

    /// 向 Agent 会话投递一帧下行消息。`Ok(true)` 已投递（发送器仍持有）；
    /// `Ok(false)` 无会话或发送失败（对端关闭/通道满——Agent 离线或不可达）。
    pub async fn send(&self, agent_id: i64, msg: ChannelMessage) -> Result<bool, SchedError> {
        let sender = self.inner.read().await.get(&agent_id).cloned();
        match sender {
            Some(tx) => tx
                .send(Ok(msg))
                .await
                .map(|_| true)
                .map_err(|_| SchedError::Dispatch("Agent 会话已断开".into())),
            None => Ok(false),
        }
    }

    /// 经通道往返：先注册待满足响应，再发指令，等上行响应帧（由 session_loop
    /// 的 `WorkspaceList`/`CacheList` 臂经 [`Self::fulfill`] 满足）。
    ///
    /// 注册先于发送——避免「发送后、注册前」响应已到却被丢的窄窗口。Agent
    /// 离线（无会话）即 [`AwaitError::Offline`]；超时即 [`AwaitError::Timeout`]；
    /// 会话中途断开（oneshot 发送器 drop）即 [`AwaitError::Offline`]。
    pub async fn send_and_await(
        &self,
        agent_id: i64,
        cmd: ChannelMessage,
        kind: AwaitKind,
    ) -> Result<ChannelMessage, AwaitError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending.write().await.insert((agent_id, kind), resp_tx);
        if !matches!(self.send(agent_id, cmd).await, Ok(true)) {
            // 离线 / 发送失败：清待满足，返回 Offline（pending 指令不发——
            // 工作区/缓存列表无离线补发，UI 重发查询即可，ADR-0011/0012）。
            self.pending.write().await.remove(&(agent_id, kind));
            return Err(AwaitError::Offline);
        }
        match tokio::time::timeout(AWAIT_TIMEOUT, resp_rx).await {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => Err(AwaitError::Offline), // 会话断开：发送器 drop
            Err(_) => {
                self.pending.write().await.remove(&(agent_id, kind));
                Err(AwaitError::Timeout)
            }
        }
    }

    /// session_loop 收到响应帧时满足待等待的往返（无等待者即丢弃——UI 可重发）。
    async fn fulfill(&self, agent_id: i64, kind: AwaitKind, msg: ChannelMessage) {
        if let Some(tx) = self.pending.write().await.remove(&(agent_id, kind)) {
            let _ = tx.send(msg);
        }
    }
}

/// Agent 通道服务：持有组合根状态（认证面 + 心跳面 + 任务面共用 repo，与
/// REST 同装配）。
pub struct AgentChannelService {
    state: AppState,
    sessions: Arc<SessionRegistry>,
    scheduler: SchedulerHandle,
}

impl AgentChannelService {
    /// 以组合根状态构造。`sessions` 为会话注册表（任务面下发目的地），
    /// `scheduler` 为调度循环句柄（上行任务面帧转发）。
    pub fn new(
        state: AppState,
        sessions: Arc<SessionRegistry>,
        scheduler: SchedulerHandle,
    ) -> Self {
        Self {
            state,
            sessions,
            scheduler,
        }
    }
}

/// 生产下发端口：把调度匹配结果经真实通道会话下发（sched 的
/// [`JobDispatcher`] trait 实现）。grpc 服务与调度循环共享同一个
/// `SessionRegistry`。
pub struct GrpcDispatcher {
    sessions: Arc<SessionRegistry>,
}

impl GrpcDispatcher {
    /// 以会话注册表构造（与 [`AgentChannelService`] 同源）。
    pub fn new(sessions: Arc<SessionRegistry>) -> Self {
        Self { sessions }
    }
}

#[tonic::async_trait]
impl JobDispatcher for GrpcDispatcher {
    async fn dispatch_job(&self, agent_id: i64, job: &JobRow) -> Result<bool, SchedError> {
        // JobSpec 组装：从 spec 快照映射到 proto（下发期字段 job_id/
        // log_limit_bytes 在快照之外，此处补）。
        let Some(spec_json) = &job.spec_json else {
            return Err(SchedError::Dispatch("任务无 spec 快照".into()));
        };
        let spec: ResolvedJobSpec = serde_json::from_str(spec_json)
            .map_err(|e| SchedError::Dispatch(format!("spec 快照损坏：{e}")))?;
        let msg = ChannelMessage {
            kind: Some(Kind::JobSpec(Box::new(job_spec_message(job, &spec)))),
        };
        self.sessions.send(agent_id, msg).await
    }

    async fn cancel_job(
        &self,
        agent_id: i64,
        build_id: i64,
        job_id: i64,
    ) -> Result<(), SchedError> {
        let msg = ChannelMessage {
            kind: Some(Kind::Cancel(CancelBuild {
                build_id: build_id.to_string(),
                job_id: job_id.to_string(),
            })),
        };
        self.sessions.send(agent_id, msg).await?;
        Ok(())
    }

    async fn dispatch_upgrade(
        &self,
        agent_id: i64,
        cmd: UpgradeCommand,
    ) -> Result<bool, SchedError> {
        // 升级指令经通道下发（ADR-0017）：Agent 自排空 → 下载校验 → 原子换入
        // → spawn 重启，状态经 UpgradeStatus 上报。离线 Agent 由调度侧
        // 持久化指令、重连补发（与取消指令同机制）。
        let msg = ChannelMessage {
            kind: Some(Kind::Upgrade(cmd)),
        };
        self.sessions.send(agent_id, msg).await
    }
}

/// ResolvedJobSpec → proto JobSpec（ADR-0009：Agent 拿到即执行，对 Pipeline
/// 定义一无所知）。`job_id` 为任务行 id（JobStatus/JobAck 的寻径键）。
pub fn job_spec_message(job: &JobRow, spec: &ResolvedJobSpec) -> sisyphus_proto::agent::JobSpec {
    use sisyphus_proto::agent::{
        ArtifactDownload as PDownload, ArtifactUpload as PUpload, CacheSpec as PCache,
        CheckoutStep, ContainerEnv, ExecutionEnv as PExecEnv, JobStep, ShellStep,
    };
    let env = spec
        .env
        .iter()
        .map(|e| (e.name.clone(), e.value.clone()))
        .collect();
    let steps = spec
        .steps
        .iter()
        .map(|step| JobStep {
            name: match step {
                ResolvedStep::Shell { seq, .. } | ResolvedStep::Checkout { seq, .. } => {
                    format!("step-{seq}")
                }
            },
            seq: match step {
                ResolvedStep::Shell { seq, .. } | ResolvedStep::Checkout { seq, .. } => *seq,
            },
            kind: match step {
                ResolvedStep::Shell { command, .. } => {
                    Some(sisyphus_proto::agent::job_step::Kind::Shell(ShellStep {
                        command: command.clone(),
                    }))
                }
                ResolvedStep::Checkout {
                    scm, submodules, ..
                } => Some(sisyphus_proto::agent::job_step::Kind::Checkout(
                    CheckoutStep {
                        vcs: match scm.vcs {
                            Vcs::Git => sisyphus_proto::agent::VcsType::VcsGit as i32,
                            Vcs::Svn => sisyphus_proto::agent::VcsType::VcsSvn as i32,
                        },
                        repo_url: scm.repo_url.clone(),
                        r#ref: scm.branch.clone(),
                        commit: scm.commit.clone(),
                        submodules: *submodules,
                    },
                )),
            },
        })
        .collect();
    let exec_env = match &spec.exec_env {
        sisyphus_model::pipeline::ExecutionEnv::Host => Some(PExecEnv {
            kind: Some(sisyphus_proto::agent::execution_env::Kind::Host(
                sisyphus_proto::agent::HostEnv {},
            )),
        }),
        sisyphus_model::pipeline::ExecutionEnv::Container { image } => Some(PExecEnv {
            kind: Some(sisyphus_proto::agent::execution_env::Kind::Container(
                ContainerEnv {
                    image: image.clone(),
                },
            )),
        }),
    };
    sisyphus_proto::agent::JobSpec {
        job_id: job.id.to_string(),
        pipeline_name: spec.pipeline_name.clone(),
        job_name: spec.job_name.clone(),
        build_number: spec.build_number,
        attempt: spec.attempt,
        log_limit_bytes: 0, // 默认 50MB 由 Agent 侧裁决（ADR-0013）
        steps,
        env,
        exec_env,
        timeout_minutes: spec.timeout_minutes,
        uploads: spec
            .artifact_uploads
            .iter()
            .map(|u| PUpload {
                name: u.name.clone(),
                path: u.path.clone(),
            })
            .collect(),
        downloads: spec
            .artifact_downloads
            .iter()
            .map(|d| PDownload {
                job_id: d.job.clone(),
                name: d.name.clone(),
                path: d.path.clone(),
            })
            .collect(),
        caches: spec
            .caches
            .iter()
            .map(|c| PCache {
                key: c.key.clone(),
                paths: c.paths.clone(),
                files: c.files.clone(),
            })
            .collect(),
        secrets: spec.secrets.clone(),
        scm_credential: None, // ADR-0015：SCM 凭据递送随 runner 批次
        labels: spec.labels.clone(),
        retry_count: spec.retry_count as i32,
        allow_failure: spec.allow_failure,
    }
}

/// 把 AgentChannel 服务挂到 tonic 路由上（注入组合根状态 + 会话注册表 +
/// 调度句柄）。
pub fn service(
    state: AppState,
    sessions: Arc<SessionRegistry>,
    scheduler: SchedulerHandle,
) -> AgentChannelServer<AgentChannelService> {
    AgentChannelServer::new(AgentChannelService::new(state, sessions, scheduler))
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
        let agent_version = agent_version
            .ok_or_else(|| Status::invalid_argument("首帧必须是握手（含 Agent 版本号）"))?;

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

        // 版本进契约 + 升级态收敛（ADR-0017）：握手上报版本落库；若版本已变
        // （升级成功，新进程以新版本重连）或上次停在 restarting（新进程重连，
        // RESTARTING 报告可能在旧进程退出前丢失）则清升级态与待补发指令——
        // 升级已终结，回到「可派发」。首连（prev 版本为空）不清：待补发指令
        // 留给 on_job_reported 补发。
        let new_version = AgentVersion::from_proto(&agent_version);
        let prev_version = agent.agent_version().unwrap_or(None);
        self.state
            .agents
            .set_agent_version(agent.id, &new_version)
            .await
            .map_err(|e| Status::internal(format!("版本落库失败：{e}")))?;
        let upgraded = prev_version.is_some_and(|p| p != new_version)
            || agent.upgrade_phase.as_deref() == Some("restarting");
        if upgraded {
            if let Err(e) = self.state.agents.clear_upgrade_state(agent.id).await {
                tracing::warn!(agent = %agent.name, error = %e, "清升级态失败");
            }
            if let Err(e) = self.state.agents.clear_pending_upgrade(agent.id).await {
                tracing::warn!(agent = %agent.name, error = %e, "清待补发升级指令失败");
            }
        }

        tracing::info!(agent = %agent.name, "agent connected（通道认证通过）");

        let (tx, rx) = mpsc::channel(SESSION_TX_CAPACITY);
        // 会话注册：JobSpec/CancelBuild 的下发目的地（重连覆盖旧会话）。
        self.sessions.register(agent.id, tx.clone()).await;
        // 在线事件（sched 据此匹配等待中的任务）。
        self.state.bus.publish(Event::AgentOnline {
            agent_id: agent.id,
            name: agent.name.clone(),
        });

        // 会话任务：回发握手确认 → 逐帧处理（心跳落库 + 停用踢线复核 +
        // 任务面转发）；对端关流/断开或停用即退出（下线由 45s 扫描兜底，
        // 通道断开不抢跑）。
        tokio::spawn(session_loop(
            self.state.clone(),
            agent.name.clone(),
            agent.id,
            token_hash,
            labels_json,
            self.sessions.clone(),
            self.scheduler.clone(),
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
/// 在线/标签/磁盘占用（停用不生效即断开）；任务面帧（JobAck/JobStatus/
/// JobReported）转发调度循环；其余帧只复核 token 仍有效（停用/吊销的
/// Agent 下一请求即断开）。
#[allow(clippy::too_many_arguments)]
async fn session_loop(
    state: AppState,
    agent: String,
    agent_id: i64,
    token_hash: String,
    labels_json: String,
    sessions: Arc<SessionRegistry>,
    scheduler: SchedulerHandle,
    handshake_reply: ChannelMessage,
    mut inbound: Streaming<ChannelMessage>,
    tx: mpsc::Sender<Result<ChannelMessage, Status>>,
) {
    if tx.send(Ok(handshake_reply)).await.is_err() {
        sessions.unregister(agent_id).await;
        crate::metrics::record_grpc_disconnect("handshake_fail");
        return; // 对端已断开，会话无意义
    }

    loop {
        let msg = match read_inbound(&mut inbound).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                // 对端关流（正常下线路径）。
                crate::metrics::record_grpc_disconnect("disconnect");
                break;
            }
            Err(_) => {
                // 读帧失败（连接被重置等）：下线归 45s 扫描。
                crate::metrics::record_grpc_disconnect("read_error");
                break;
            }
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
                    crate::metrics::record_grpc_disconnect("disabled");
                    break;
                }
            }
            Some(Kind::JobAck(ack)) => {
                // 任务回执：槽位占用确认 / 拒绝释放（调度侧落库裁决）。
                let job_id = ack.job_id.parse().unwrap_or(0);
                if let Err(e) = scheduler
                    .on_job_ack(agent_id, job_id, ack.accepted, ack.error)
                    .await
                {
                    tracing::warn!(agent = %agent, error = %e, "JobAck 处理失败");
                }
            }
            Some(Kind::JobStatus(status)) => {
                // 任务状态上报：running/unknown/终态 → 调度落库 + engine 推进。
                let job_id = status.job_id.parse().unwrap_or(0);
                // 契约未知阶段（未来 Server 的新字段）：忽略不上报（旧 Server
                // 前瞻兼容，不误标 unknown）。
                if let Some(phase) = map_job_phase(status.phase)
                    && let Err(e) = scheduler
                        .on_job_status(agent_id, job_id, phase, status.exit_code, status.detail)
                        .await
                {
                    tracing::warn!(agent = %agent, error = %e, "JobStatus 处理失败");
                }
            }
            Some(Kind::JobReported(reported)) => {
                // 在途任务上报：重连重建调度状态 + 补发挂起取消。
                if let Err(e) = scheduler.on_job_reported(agent_id, reported.job_ids).await {
                    tracing::warn!(agent = %agent, error = %e, "JobReported 处理失败");
                }
            }
            Some(Kind::LogBatch(batch)) => {
                // 日志流（票 #73，ADR-0013）：落库 + 事件总线广播（SSE 尾随
                // 热通知；可丢，重放兑底走 DB）。
                if let Err(e) = handle_log_batch(&state, agent_id, batch).await {
                    tracing::warn!(agent = %agent, error = %e, "LogBatch 落库失败");
                }
            }
            Some(Kind::UpgradeStatus(status)) => {
                // 升级状态上报（ADR-0017）：首条回执 = 指令已送达 → 清待补发
                // （停止离线补发，避免对正在升级的 Agent 重发非幂等指令）；
                // 落当前阶段。UNSPECIFIED/未知 → 清升级态（复位）。
                if let Err(e) = state.agents.clear_pending_upgrade(agent_id).await {
                    tracing::warn!(agent = %agent, error = %e, "清待补发升级指令失败");
                }
                match map_upgrade_phase(status.phase) {
                    Some(phase) => {
                        // 空 error 不落（COALESCE 保留旧 error——如下载失败后再报
                        // 同阶段不带 error 不清原因）。
                        let error = (!status.error.is_empty()).then_some(status.error.as_str());
                        if let Err(e) = state
                            .agents
                            .set_upgrade_status(agent_id, phase, error)
                            .await
                        {
                            tracing::warn!(agent = %agent, error = %e, "升级状态落库失败");
                        }
                    }
                    None => {
                        if let Err(e) = state.agents.clear_upgrade_state(agent_id).await {
                            tracing::warn!(agent = %agent, error = %e, "清升级态失败");
                        }
                    }
                }
            }
            Some(Kind::WorkspaceList(list)) => {
                // 工作区列表响应（REST 经 send_and_await 往返）：满足等待者；
                // 无等待者（UI 已超时放弃）即丢弃——列表响应无离线补发，重发查询即可。
                sessions
                    .fulfill(
                        agent_id,
                        AwaitKind::WorkspaceList,
                        ChannelMessage {
                            kind: Some(Kind::WorkspaceList(list)),
                        },
                    )
                    .await;
            }
            Some(Kind::CacheList(list)) => {
                // 缓存列表响应：同工作区列表。
                sessions
                    .fulfill(
                        agent_id,
                        AwaitKind::CacheList,
                        ChannelMessage {
                            kind: Some(Kind::CacheList(list)),
                        },
                    )
                    .await;
            }
            _ => {
                // 兜底臂：UpgradeStatus / WorkspaceList / CacheList 已在上臂处理，
                // 此处只接未知/契约外帧（演进只加字段、旧 Server 忽略新 kind）。
                // 顺带做「下一请求即拒」的踢线复核：查库失败（瞬态 IO）不断健康
                // 会话——只有「明确查到且已停用/不存在」才踢线。
                match state.agents.find_active_by_hash(&token_hash).await {
                    Ok(None) => {
                        tracing::info!(agent = %agent, "agent 已停用/吊销：断开会话");
                        crate::metrics::record_grpc_disconnect("disabled");
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
    sessions.unregister(agent_id).await;
}

/// proto `UpgradePhase` → 升级阶段字符串（落库 `agents.upgrade_phase`）。
/// `None` = UNSPECIFIED/未知（复位：清升级态）。fallback 不在「升级中」集合
/// （[`crate::store::agents::AgentRow::mid_upgrade`]）→ 落 fallback 后 Agent
/// 仍可派发（退回旧版本继续跑），UI 显示「退回」+ error。
fn map_upgrade_phase(phase: i32) -> Option<&'static str> {
    use sisyphus_proto::agent::UpgradePhase as P;
    match P::try_from(phase) {
        Ok(P::UpgradeDraining) => Some("draining"),
        Ok(P::UpgradeDownloading) => Some("downloading"),
        Ok(P::UpgradeSwapping) => Some("swapping"),
        Ok(P::UpgradeRestarting) => Some("restarting"),
        Ok(P::UpgradeFallback) => Some("fallback"),
        _ => None, // UNSPECIFIED/未知：复位
    }
}

/// proto JobPhase → 任务状态。`None` = 契约未知阶段（契约演进只加字段，
/// 旧 Server 忽略新阶段——不落库、不误标 unknown、不启动宽限计时）。
fn map_job_phase(phase: i32) -> Option<crate::store::jobs::JobStatus> {
    use sisyphus_proto::agent::JobPhase as P;
    match P::try_from(phase) {
        Ok(P::JobRunning) => Some(crate::store::jobs::JobStatus::Running),
        Ok(P::JobUnknown) => Some(crate::store::jobs::JobStatus::Unknown),
        Ok(P::JobSucceeded) => Some(crate::store::jobs::JobStatus::Succeeded),
        Ok(P::JobFailed) => Some(crate::store::jobs::JobStatus::Failed),
        Ok(P::JobCancelled) => Some(crate::store::jobs::JobStatus::Cancelled),
        Ok(P::JobTimeout) => Some(crate::store::jobs::JobStatus::Timeout),
        Ok(P::JobAborted) => Some(crate::store::jobs::JobStatus::Aborted),
        _ => None, // 未知阶段：忽略（契约演进只加字段，旧 Server 前瞻兼容）
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
                // 离线事件（sched 据此转 unknown + 匹配重算）。
                state.bus.publish(Event::AgentOffline {
                    agent_id: agent.id,
                    name: agent.name.clone(),
                });
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

/// proto LogBatch 落库：任务行校验（存在 + 归属本 Agent——与 on_job_status
/// 同纪律，不越权写他人任务日志）→ proto 事件映射为 server 日志事件模型 →
/// 编码 gzip chunk → [`LogStore::append`]（按 start_seq 幂等，断线补传不重
/// 不乱序）→ 广播 [`Event::LogAppended`]（SSE 尾随热通知）。
async fn handle_log_batch(
    state: &AppState,
    agent_id: i64,
    batch: sisyphus_proto::agent::LogBatch,
) -> Result<(), crate::store::StoreError> {
    let Ok(job_id) = batch.job_id.parse::<i64>() else {
        // 契约外 job_id（非数字）：丢弃记日志，不断会话（日志面非致命）。
        tracing::warn!(agent_id, job = %batch.job_id, "LogBatch job_id 非数字，丢弃");
        return Ok(());
    };
    let Some(job) = crate::store::jobs::JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
    else {
        tracing::warn!(agent_id, job_id, "LogBatch 任务行不存在，丢弃");
        return Ok(());
    };
    if job.agent_id != Some(agent_id) {
        // 与 on_job_status 同纪律：非本 Agent 的任务，静默忽略（不越权写）。
        return Ok(());
    }
    let events = log_events_from_proto(&batch);
    if events.is_empty() {
        return Ok(());
    }
    let loc = crate::logs::location(job.build_id, job_id, batch.attempt);
    let chunk = crate::logs::encode_chunk(&events);
    state.logs.append(loc, vec![chunk]).await?;
    state.bus.publish(Event::LogAppended {
        build_id: job.build_id,
        job_id,
        attempt: batch.attempt,
    });
    Ok(())
}

/// proto `LogEvent` 序列 → server 日志事件模型（ADR-0013）：
/// - `OutputChunk` → 输出块（stream 标记；字节 UTF-8 有损解码——SSE/JSON
///   传输面是文本，ANSI 色码原样保留）；
/// - `StepEvent`（exit_code 空）→ step start（命令回显）；Some → step end
///   （退出码 + 耗时）；步骤名 proto 不携带（v1 恒空，前端回落「步骤 N」）；
/// - `Truncated` → 截断标记（limit_bytes 取全局默认上限；dropped_bytes
///   随行携带作信息面）；
/// - 契约未知 kind（None）跳过——演进只加字段，旧事件形态不炸。
fn log_events_from_proto(
    batch: &sisyphus_proto::agent::LogBatch,
) -> Vec<crate::logs::LogStreamEvent> {
    use sisyphus_proto::agent::Stream;
    use sisyphus_proto::agent::log_event::Kind as EventKind;

    batch
        .events
        .iter()
        .filter_map(|e| match e.kind.as_ref()? {
            EventKind::Output(o) => Some(crate::logs::LogStreamEvent::Output {
                seq: e.seq,
                stream: if o.stream == Stream::Stderr as i32 {
                    crate::logs::LogStream::Stderr
                } else {
                    crate::logs::LogStream::Stdout
                },
                text: String::from_utf8_lossy(&o.data).into_owned(),
            }),
            EventKind::Step(s) => match s.exit_code {
                None => Some(crate::logs::LogStreamEvent::StepStart {
                    seq: e.seq,
                    step: s.seq,
                    name: String::new(),
                    command: s.command.clone(),
                    started_at: s.step_started_at_ms,
                }),
                Some(exit_code) => Some(crate::logs::LogStreamEvent::StepEnd {
                    seq: e.seq,
                    step: s.seq,
                    exit_code: Some(exit_code),
                    duration_ms: s.step_ended_at_ms - s.step_started_at_ms,
                }),
            },
            EventKind::Truncated(t) => Some(crate::logs::LogStreamEvent::Truncated {
                seq: e.seq,
                limit_bytes: crate::logs::DEFAULT_LOG_LIMIT_BYTES,
                dropped_bytes: t.dropped_bytes,
            }),
        })
        .collect()
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

    #[test]
    fn log_events_map_output_step_and_truncated() {
        use sisyphus_proto::agent::log_event::Kind as EventKind;
        use sisyphus_proto::agent::{LogEvent, OutputChunk, StepEvent, Stream, Truncated};

        let batch = sisyphus_proto::agent::LogBatch {
            job_id: "7".into(),
            attempt: 1,
            start_seq: 0,
            events: vec![
                LogEvent {
                    seq: 0,
                    kind: Some(EventKind::Output(OutputChunk {
                        stream: Stream::Stdout as i32,
                        data: b"hello \x1b[32mworld\x1b[0m\n".to_vec(),
                    })),
                },
                LogEvent {
                    seq: 1,
                    kind: Some(EventKind::Output(OutputChunk {
                        stream: Stream::Stderr as i32,
                        data: b"boom".to_vec(),
                    })),
                },
                LogEvent {
                    seq: 2,
                    kind: Some(EventKind::Step(StepEvent {
                        seq: 3,
                        step_started_at_ms: 1000,
                        step_ended_at_ms: 0,
                        exit_code: None,
                        command: "cargo build".into(),
                    })),
                },
                LogEvent {
                    seq: 3,
                    kind: Some(EventKind::Step(StepEvent {
                        seq: 3,
                        step_started_at_ms: 1000,
                        step_ended_at_ms: 1250,
                        exit_code: Some(0),
                        command: String::new(),
                    })),
                },
                LogEvent {
                    seq: 4,
                    kind: Some(EventKind::Truncated(Truncated {
                        dropped_bytes: 4096,
                    })),
                },
                LogEvent { seq: 5, kind: None }, // 契约未知 kind：跳过
            ],
        };
        assert_eq!(
            log_events_from_proto(&batch),
            vec![
                crate::logs::LogStreamEvent::Output {
                    seq: 0,
                    stream: crate::logs::LogStream::Stdout,
                    text: "hello \x1b[32mworld\x1b[0m\n".into(),
                },
                crate::logs::LogStreamEvent::Output {
                    seq: 1,
                    stream: crate::logs::LogStream::Stderr,
                    text: "boom".into(),
                },
                crate::logs::LogStreamEvent::StepStart {
                    seq: 2,
                    step: 3,
                    name: String::new(),
                    command: "cargo build".into(),
                    started_at: 1000,
                },
                crate::logs::LogStreamEvent::StepEnd {
                    seq: 3,
                    step: 3,
                    exit_code: Some(0),
                    duration_ms: 250,
                },
                crate::logs::LogStreamEvent::Truncated {
                    seq: 4,
                    limit_bytes: crate::logs::DEFAULT_LOG_LIMIT_BYTES,
                    dropped_bytes: 4096,
                },
            ]
        );
    }
}

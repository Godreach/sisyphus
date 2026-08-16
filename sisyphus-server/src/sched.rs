//! sched 模块：调度与下发（票 B2c-T4，ADR-0008，消费 #46 engine 状态机与
//! #47 Agent 在线/标签/槽位数据源）。
//!
//! **事件驱动单调度循环**（[`Scheduler::run`]）：进程内一条 tokio 任务，
//! 事件唤醒——事件总线（job 就绪 / 槽位释放 / Agent 上下线 / 构建创建 /
//! fail-fast 构建终态）+ grpc 任务面（JobAck / JobStatus / JobReported）+
//! 周期 tick（超时 / 宽限 / 匹配兜底）。排队状态全落 SQLite（无内存队列），
//! 启动从库重建（[`Scheduler::reconstruct`]：running/queued/unknown 任务、
//! 挂起取消补发）。
//!
//! **匹配语义**：全局 pending 池按就绪时间 FIFO（[`JobRepo::pending_pool`]），
//! 对每个 job 找「在线 + 有空槽 + 标签 AND 全集」的 Agent
//! （[`AgentRepo::match_candidates`]；容器任务隐式容器标签已由 engine 组装
//! 进 labels）。无匹配 → 无限等待 + 标注缺失标签（`waiting_detail`，供 UI
//! 警示态）。
//!
//! **槽位**：per-Agent `max_concurrency`（默认 1）Server 端中心化计数
//! （[`AgentRepo::has_slots`]），从下发（dispatch）占到任务终态——running/
//! unknown 占槽（[`JobStatus::occupies_slot`]）。
//!
//! **job 超时**（分钟，0=无限）从下发计时（`started_at`），超时走取消路径
//! 终态 `timeout`（[`Self::timeout_pass`]）。**build 级取消**（[`Self::cancel_build`]）：
//! 排队中移出 pending 池、运行中下发 CancelBuild、离线挂起重连补发（DB 可
//! 重建：`channel_cancel_pending` 视图）。**fail-fast 级联**走同一通道：engine
//! 级联置 cancelled 后，本循环在构建 failed 事件上补发 CancelBuild。
//!
//! **Agent 离线**（45s 判离线由 grpc sweep 发布 `Event::AgentOffline`）：
//! 运行中任务转 unknown（重连回归 running，unknown_at 清空）、orphan 宽限
//! （config `[scheduler]`，默认 10 分钟）超时判 failed（[`Self::orphan_pass`]）；
//! Agent 重启丢任务上报 aborted 判失败（重连 JobReported 对账）。
//!
//! **下发经 [`JobDispatcher`] trait 插拔**：生产面是 grpc 的通道会话（真实
//! tonic，见 `grpc::GrpcDispatcher`），测试面注入内存 fake——proto 缝 fake
//! Agent 闭环在真实 tonic 装配下驱动本模块。

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};

use crate::engine::Engine;
use crate::events::{Event, EventBus};
use crate::store::agents::{AgentRepo, AgentRow};
use crate::store::builds::{BuildRepo, BuildStatus};
use crate::store::jobs::{JobRepo, JobRow, JobStatus};
use crate::store::projects::ProjectRepo;
use crate::store::{StoreError, now_ms};

/// 调度循环周期 tick（超时/宽限/匹配兜底唤醒；秒级整秒，sleep 用）。
const LOOP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// 调度循环内部事件队列容量（任务面帧 + 取消请求；循环处理快，bounded 即
/// 背压——满了上游 session_loop 等待，不丢帧）。
const LOOP_CAPACITY: usize = 256;

/// 调度面错误。
#[derive(Debug)]
pub enum SchedError {
    /// 存储层错误。
    Store(StoreError),
    /// 通道投递失败（Agent 会话不存在/断开）。
    Dispatch(String),
    /// 调度循环已退出（取消请求无法完成）。
    Shutdown,
}

impl std::fmt::Display for SchedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedError::Store(e) => write!(f, "存储错误：{e}"),
            SchedError::Dispatch(what) => write!(f, "下发失败：{what}"),
            SchedError::Shutdown => write!(f, "调度循环已退出"),
        }
    }
}

impl std::error::Error for SchedError {}

impl From<StoreError> for SchedError {
    fn from(e: StoreError) -> Self {
        SchedError::Store(e)
    }
}

/// 下发端口（插拔缝）：生产面是 grpc 的通道会话（真实 tonic），测试面注入
/// 内存 fake（proto 缝闭环）。
#[tonic::async_trait]
pub trait JobDispatcher: Send + Sync {
    /// 下发 JobSpec 到 Agent 当前会话。`Ok(true)` 已投递（等 ack 确认）；
    /// `Ok(false)` Agent 无会话（离线）——调度侧回收槽位、回池重排。
    async fn dispatch_job(&self, agent_id: i64, job: &JobRow) -> Result<bool, SchedError>;
    /// 下发 CancelBuild（build 级 + job_id）。
    async fn cancel_job(&self, agent_id: i64, build_id: i64, job_id: i64)
    -> Result<(), SchedError>;
}

/// 调度循环的内部事件（grpc 任务面 → 单循环串行化入口）。
enum LoopEvent {
    /// 任务回执（槽位占用确认 / 拒绝释放）。
    JobAck {
        agent_id: i64,
        job_id: i64,
        accepted: bool,
        error: String,
    },
    /// 任务状态上报（running/unknown 与终态）。
    JobStatus {
        agent_id: i64,
        job_id: i64,
        status: JobStatus,
        exit_code: Option<i32>,
        detail: String,
    },
    /// 在途任务上报（重连重建 + 补发挂起取消）。
    JobReported { agent_id: i64, job_ids: Vec<String> },
    /// 取消构建（REST 面入口；`done` 为完成信号）。
    CancelBuild {
        build_id: i64,
        done: Option<oneshot::Sender<Result<(), SchedError>>>,
    },
}

/// 调度器句柄（grpc 面转发任务事件、REST 面取消构建用）。`Clone` 供多消费方。
#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<LoopEvent>,
}

impl SchedulerHandle {
    /// 丢弃面句柄：任务面帧不转发（握手/心跳测试面用；发送即失败——测试
    /// 不驱动调度循环时用此构造，避免持有真实循环引用）。
    pub fn discard() -> Self {
        let (tx, _rx) = mpsc::channel(16);
        Self { tx }
    }

    /// 任务回执转发。
    pub async fn on_job_ack(
        &self,
        agent_id: i64,
        job_id: i64,
        accepted: bool,
        error: String,
    ) -> Result<(), SchedError> {
        self.tx
            .send(LoopEvent::JobAck {
                agent_id,
                job_id,
                accepted,
                error,
            })
            .await
            .map_err(|_| SchedError::Shutdown)
    }

    /// 任务状态上报转发。
    pub async fn on_job_status(
        &self,
        agent_id: i64,
        job_id: i64,
        status: JobStatus,
        exit_code: Option<i32>,
        detail: String,
    ) -> Result<(), SchedError> {
        self.tx
            .send(LoopEvent::JobStatus {
                agent_id,
                job_id,
                status,
                exit_code,
                detail,
            })
            .await
            .map_err(|_| SchedError::Shutdown)
    }

    /// 在途任务上报转发。
    pub async fn on_job_reported(
        &self,
        agent_id: i64,
        job_ids: Vec<String>,
    ) -> Result<(), SchedError> {
        self.tx
            .send(LoopEvent::JobReported { agent_id, job_ids })
            .await
            .map_err(|_| SchedError::Shutdown)
    }

    /// 取消构建（build 级）：排队中移出、运行中下发 CancelBuild。等待调度
    /// 循环完成（REST 端点需同步结果）。
    pub async fn cancel_build(&self, build_id: i64) -> Result<(), SchedError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.tx
            .send(LoopEvent::CancelBuild {
                build_id,
                done: Some(done_tx),
            })
            .await
            .map_err(|_| SchedError::Shutdown)?;
        done_rx.await.map_err(|_| SchedError::Shutdown)?
    }
}

/// 进程内单调度循环：事件驱动匹配/超时/宽限/取消，状态全落 SQLite。
pub struct Scheduler {
    engine: Engine,
    builds: BuildRepo,
    jobs: JobRepo,
    agents: AgentRepo,
    projects: ProjectRepo,
    dispatcher: Arc<dyn JobDispatcher>,
    /// orphan 宽限时长（毫秒）。
    orphan_grace_ms: i64,
    /// 循环的事件入口（grpc 面持有 handle 转发；run 持有 receiver）。
    tx: mpsc::Sender<LoopEvent>,
    rx: Option<mpsc::Receiver<LoopEvent>>,
}

impl Scheduler {
    /// 装配：engine（编排推进）+ 下发端口（grpc 会话面）+ orphan 宽限分钟。
    pub fn new(
        engine: Engine,
        pool: SqlitePool,
        dispatcher: Arc<dyn JobDispatcher>,
        orphan_grace_minutes: i64,
    ) -> Self {
        let (tx, rx) = mpsc::channel(LOOP_CAPACITY);
        Self {
            engine,
            builds: BuildRepo::new(pool.clone()),
            jobs: JobRepo::new(pool.clone()),
            agents: AgentRepo::new(pool.clone()),
            projects: ProjectRepo::new(pool.clone()),
            dispatcher,
            orphan_grace_ms: orphan_grace_minutes.max(0) * 60_000,
            tx,
            rx: Some(rx),
        }
    }

    /// 调度器句柄（grpc 面转发任务事件、REST 面取消构建）。
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            tx: self.tx.clone(),
        }
    }

    /// 事件驱动主循环：bus 事件 + grpc 任务面事件 + 周期 tick（超时/宽限/
    /// 匹配兜底）。运行期不返回（循环进程级生命周期）。
    pub async fn run(mut self, bus: EventBus) {
        let mut rx = self.rx.take().expect("run 只调一次");
        let mut bus_rx = bus.subscribe();
        let mut ticker = tokio::time::interval(LOOP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                maybe_ev = rx.recv() => {
                    match maybe_ev {
                        Some(ev) => self.handle_loop_event(ev).await,
                        None => break, // 全部发送方已弃（grpc 面关停）
                    }
                }
                maybe_bus = bus_rx.recv() => {
                    // Lagged/Closed：可丢热通知，靠周期 tick 兜底。
                    if let Ok(ev) = maybe_bus {
                        self.handle_bus_event(ev).await;
                    }
                }
                _ = ticker.tick() => {
                    self.periodic_pass(now_ms()).await;
                }
            }
        }
        tracing::info!("调度循环退出");
    }

    // -----------------------------------------------------------------------
    // 事件处理
    // -----------------------------------------------------------------------

    async fn handle_loop_event(&self, ev: LoopEvent) {
        let now = now_ms();
        let result = match ev {
            LoopEvent::JobAck {
                agent_id,
                job_id,
                accepted,
                error,
            } => {
                self.on_job_ack(agent_id, job_id, accepted, &error, now)
                    .await
            }
            LoopEvent::JobStatus {
                agent_id,
                job_id,
                status,
                exit_code,
                detail,
            } => {
                self.on_job_status(agent_id, job_id, status, exit_code, &detail, now)
                    .await
            }
            LoopEvent::JobReported { agent_id, job_ids } => {
                self.on_job_reported(agent_id, &job_ids, now).await
            }
            LoopEvent::CancelBuild { build_id, done } => {
                let result = self.cancel_build(build_id).await;
                if let Some(done) = done {
                    let _ = done.send(result.map_err(SchedError::from));
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "调度循环事件处理失败");
        }
    }

    async fn handle_bus_event(&self, ev: Event) {
        match ev {
            Event::BuildCreated { build_id, .. } => {
                // 新构建入队：drive 放行排队（FIFO）+ 组装阶段任务（发布
                // queued JobStatus 事件 → 匹配下发）。
                if let Err(e) = self.engine.drive(build_id).await {
                    tracing::warn!(build_id, error = %e, "构建推进失败");
                }
            }
            Event::JobStatus { .. } => {
                // 任务状态迁移：槽位释放/新任务入池唤醒匹配。
                if let Err(e) = self.match_pass(now_ms()).await {
                    tracing::warn!(error = %e, "匹配扫描失败");
                }
            }
            Event::AgentOnline { .. } => {
                // 新 Agent 上线：等待中的任务可能可匹配。
                if let Err(e) = self.match_pass(now_ms()).await {
                    tracing::warn!(error = %e, "匹配扫描失败");
                }
            }
            Event::AgentOffline { agent_id, .. } => {
                // 45s 判离线：运行中任务转 unknown（离线不判死），匹配重算。
                if let Err(e) = self.on_agent_offline(agent_id, now_ms()).await {
                    tracing::warn!(agent_id, error = %e, "Agent 离线处置失败");
                }
            }
            Event::BuildStatus {
                build_id, status, ..
            } if status == BuildStatus::Failed || status == BuildStatus::Cancelled => {
                // fail-fast 级联 / 取消：已置 cancelled 的在途任务经通道补发
                // CancelBuild（离线者重连时经 JobReported 对账补发）。
                if let Err(e) = self.send_pending_cancels_for_build(build_id).await {
                    tracing::warn!(build_id, error = %e, "CancelBuild 补发失败");
                }
            }
            Event::BuildStatus { .. } => {}
        }
    }

    /// 周期兜底：匹配（标签变更/槽位释放未唤醒的场景）+ job 超时 + orphan 宽限。
    async fn periodic_pass(&self, now: i64) {
        if let Err(e) = self.match_pass(now).await {
            tracing::warn!(error = %e, "匹配扫描失败");
        }
        if let Err(e) = self.timeout_pass(now).await {
            tracing::warn!(error = %e, "超时扫描失败");
        }
        if let Err(e) = self.orphan_pass(now).await {
            tracing::warn!(error = %e, "orphan 宽限扫描失败");
        }
    }

    // -----------------------------------------------------------------------
    // 启动重建
    // -----------------------------------------------------------------------

    /// 启动从库重建（ADR-0008：Server 重启不取消在途任务）：
    /// 1. 非终态构建 drive（放行排队 + 补齐 queued 任务 spec）；
    /// 2. 挂起取消补发（cancelled/failed 构建的在途任务重发 CancelBuild）；
    /// 3. running/unknown 在途任务等 Agent 重连 JobReported 对账（此处不
    ///    动行——Agent 侧续跑，重连回归）。
    ///
    /// orphan 宽限计时从 `unknown_at`（已落库）继续，重启不重置窗口。
    pub async fn reconstruct(&self) -> Result<(), StoreError> {
        for build in self.builds.non_terminal().await? {
            self.engine.drive(build.id).await?;
        }
        for job in self.jobs.channel_cancel_pending().await? {
            if let Some(agent_id) = job.agent_id {
                let _ = self
                    .dispatcher
                    .cancel_job(agent_id, job.build_id, job.id)
                    .await;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 匹配与下发
    // -----------------------------------------------------------------------

    /// 一轮匹配：全局 pending 池 FIFO → 每个任务找候选 Agent → 下发（槽位
    /// dispatch 即占，回执失败回池）。无匹配标注等待原因（缺失标签/无在线/
    /// 无槽位）。幂等：行状态条件更新保证单实例循环下不重复下发。
    pub async fn match_pass(&self, now: i64) -> Result<(), StoreError> {
        for job in self.jobs.pending_pool().await? {
            let required: Vec<String> = serde_json::from_str(&job.labels).unwrap_or_default();
            let candidates = self.agents.match_candidates(None, &required).await?;
            if candidates.is_empty() {
                let reason = self.waiting_reason(&required).await?;
                self.jobs.set_waiting(job.id, Some(&reason)).await?;
                continue;
            }
            for agent_id in candidates {
                // 条件更新裁决槽位：只有「仍 queued」才下发成功（单循环下
                // 防重，跨进程/重启防重靠条件更新幂等）。
                if !self.jobs.dispatch(job.id, agent_id, now).await? {
                    continue;
                }
                match self.dispatcher.dispatch_job(agent_id, &job).await {
                    Ok(true) => {
                        // 已投递：等待 ack 确认（槽位已占，running 事件广播）。
                        self.publish_job_status(job.id).await;
                        break;
                    }
                    Ok(false) => {
                        // Agent 无会话（离线/会话已断）：回收槽位、试下一候选。
                        self.jobs.revert_to_queued(job.id).await?;
                        self.jobs
                            .set_waiting(job.id, Some("等待 agent 上线"))
                            .await?;
                    }
                    Err(e) => {
                        // 通道层错误（系统性问题）：回收、标注、本轮不试它。
                        self.jobs.revert_to_queued(job.id).await?;
                        self.jobs
                            .set_waiting(job.id, Some(&format!("下发失败：{e}")))
                            .await?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// 等待原因标注：缺标签 / 无在线 Agent / 在线 Agent 无空槽（ADR-0019
    /// 「无匹配 Agent/缺标签」分类的输入面）。
    async fn waiting_reason(&self, required: &[String]) -> Result<String, StoreError> {
        let all = self.agents.list().await?;
        let online: Vec<&AgentRow> = all.iter().filter(|a| a.online && !a.disabled).collect();
        if online.is_empty() {
            return Ok("等待匹配 agent：无在线 agent".into());
        }
        let mut online_labels: HashSet<String> = HashSet::new();
        for agent in online {
            online_labels.extend(agent.all_labels()?);
        }
        let missing: Vec<&String> = required
            .iter()
            .filter(|r| !online_labels.contains(*r))
            .collect();
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
            return Ok(format!("等待匹配 agent：缺标签 {}", names.join(", ")));
        }
        Ok("等待匹配 agent：在线 agent 无空槽".into())
    }

    // -----------------------------------------------------------------------
    // 任务面（JobAck / JobStatus / JobReported）
    // -----------------------------------------------------------------------

    /// 任务回执：接受 → 槽位已占（dispatch 已置 running），无动作；拒绝 →
    /// 回收槽位、回池重排、标注等待原因。
    async fn on_job_ack(
        &self,
        agent_id: i64,
        job_id: i64,
        accepted: bool,
        error: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let Some(job) = self.jobs.get(job_id).await? else {
            return Ok(());
        };
        if job.agent_id != Some(agent_id) {
            return Ok(()); // 防错报：非本 Agent 的回执忽略
        }
        if accepted {
            return Ok(());
        }
        let reason = if error.is_empty() {
            "agent 拒绝任务".to_string()
        } else {
            format!("agent 拒绝任务：{error}")
        };
        self.jobs.revert_to_queued(job_id).await?;
        self.jobs.set_waiting(job_id, Some(&reason)).await?;
        self.publish_job_status(job_id).await;
        self.match_pass(now).await
    }

    /// 任务状态上报（running/unknown/终态）：行仍非终态才迁移（终态吸收）；
    /// 终态经 engine 推进（重试/级联）+ 槽位释放（后续匹配唤醒）。
    async fn on_job_status(
        &self,
        agent_id: i64,
        job_id: i64,
        status: JobStatus,
        exit_code: Option<i32>,
        detail: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let Some(job) = self.jobs.get(job_id).await? else {
            return Ok(());
        };
        if job.agent_id != Some(agent_id) {
            return Ok(());
        }
        if job.status.is_terminal() {
            return Ok(());
        }
        let detail = (!detail.is_empty()).then_some(detail);
        if self
            .jobs
            .transition(job.id, status, exit_code, detail, now)
            .await?
        {
            let fresh = self.jobs.get(job_id).await?.expect("刚迁移的行必存在");
            self.publish_job_status(job_id).await;
            if fresh.status.is_terminal() {
                self.engine.on_job_terminal(&fresh, now).await?;
            }
        }
        Ok(())
    }

    /// 在途任务上报对账（重连重建，ADR-0008）：
    /// - 上报在跑 → 回归 running（清宽限计时）；构建已取消 → 补发 CancelBuild。
    /// - 未上报且原为 unknown（离线前在跑）→ Agent 重启丢任务 → aborted
    ///   判失败（fail-fast）。
    /// - 未上报且原为 running（下发未回执/从未送达）→ 回池重排（重发
    ///   JobSpec，Agent 确认未持有故无重复执行风险）。
    /// - 挂起取消（级联/离线）经通道补发。
    async fn on_job_reported(
        &self,
        agent_id: i64,
        reported: &[String],
        now: i64,
    ) -> Result<(), StoreError> {
        let reported: HashSet<&str> = reported.iter().map(String::as_str).collect();
        for job in self.jobs.by_agent(agent_id).await? {
            let in_report = reported.contains(job.id.to_string().as_str());
            if in_report {
                if job.status != JobStatus::Running {
                    self.jobs
                        .transition(job.id, JobStatus::Running, None, None, now)
                        .await?;
                    self.publish_job_status(job.id).await;
                }
                let build = self.builds.get(job.build_id).await?;
                if build.is_some_and(|b| b.status == BuildStatus::Cancelled) {
                    let _ = self
                        .dispatcher
                        .cancel_job(agent_id, job.build_id, job.id)
                        .await;
                }
            } else if job.status == JobStatus::Unknown {
                // Agent 重启丢任务：aborted 判失败（fail-fast 级联）。
                if self
                    .jobs
                    .transition(
                        job.id,
                        JobStatus::Aborted,
                        None,
                        Some("agent 重启丢任务"),
                        now,
                    )
                    .await?
                {
                    let fresh = self.jobs.get(job.id).await?.expect("刚迁移的行必存在");
                    self.publish_job_status(job.id).await;
                    self.engine.on_job_terminal(&fresh, now).await?;
                }
            } else {
                // running 未回执未上报：从未送达 → 回池重排。
                if self.jobs.revert_to_queued(job.id).await?.is_some() {
                    self.publish_job_status(job.id).await;
                }
            }
        }
        // 级联/离线挂起的取消补发（该 Agent 的已 cancelled 在途任务）。
        for job in self.jobs.channel_cancel_pending_for_agent(agent_id).await? {
            if let Some(id) = job.agent_id {
                let _ = self.dispatcher.cancel_job(id, job.build_id, job.id).await;
            }
        }
        self.match_pass(now).await
    }

    // -----------------------------------------------------------------------
    // Agent 离线 / 超时 / 宽限 / 取消
    // -----------------------------------------------------------------------

    /// Agent 判离线处置：该 Agent 的 running 任务转 unknown（离线不判死，
    /// 重连回归），匹配重算（等待其它 Agent 的任务可改道）。
    async fn on_agent_offline(&self, agent_id: i64, now: i64) -> Result<(), StoreError> {
        self.jobs.agent_offline_to_unknown(agent_id, now).await?;
        for job in self.jobs.by_agent(agent_id).await? {
            self.publish_job_status(job.id).await;
        }
        self.match_pass(now).await
    }

    /// job 超时扫描（分钟，0=无限）：running 任务从下发计时（started_at），
    /// 超时走取消路径终态 timeout → engine 推进（重试/级联）。
    pub async fn timeout_pass(&self, now: i64) -> Result<(), StoreError> {
        for job in self.jobs.timeout_due(now).await? {
            if self
                .jobs
                .transition(job.id, JobStatus::Timeout, None, Some("job 超时"), now)
                .await?
            {
                let fresh = self.jobs.get(job.id).await?.expect("刚迁移的行必存在");
                self.publish_job_status(job.id).await;
                self.engine.on_job_terminal(&fresh, now).await?;
            }
        }
        Ok(())
    }

    /// orphan 宽限扫描：unknown 任务超宽限判 failed（避免掉线机器堵死串行
    /// 队列，ADR-0008）→ engine 推进（fail-fast）。
    pub async fn orphan_pass(&self, now: i64) -> Result<(), StoreError> {
        for job in self.jobs.unknown_jobs().await? {
            let unknown_at = job.unknown_at.unwrap_or(0);
            if now - unknown_at < self.orphan_grace_ms {
                continue;
            }
            if self.jobs.mark_orphan_failed(job.id, now).await? {
                let fresh = self.jobs.get(job.id).await?.expect("刚迁移的行必存在");
                self.publish_job_status(job.id).await;
                self.engine.on_job_terminal(&fresh, now).await?;
            }
        }
        Ok(())
    }

    /// 取消构建（build 级）：排队中移出 pending 池、运行中下发 CancelBuild、
    /// 构建置 cancelled（终态吸收幂等）。
    pub async fn cancel_build(&self, build_id: i64) -> Result<(), StoreError> {
        let now = now_ms();
        let Some(build) = self.builds.get(build_id).await? else {
            return Err(StoreError::NotFound(format!("构建 {build_id} 不存在")));
        };
        if build.status.is_terminal() {
            return Ok(()); // 已终态：幂等
        }
        let was_running = build.status == BuildStatus::Running;
        self.builds
            .transition(build_id, BuildStatus::Cancelled, now)
            .await?;
        self.jobs.cancel_queued_by_build(build_id, now).await?;
        if was_running {
            for job in self.jobs.in_flight_by_build(build_id).await? {
                if let Some(agent_id) = job.agent_id {
                    let _ = self.dispatcher.cancel_job(agent_id, build_id, job.id).await;
                }
            }
        }
        // 广播：排队取消任务 + 构建终态。
        for job in self.jobs.list_by_build(build_id).await? {
            if job.status == JobStatus::Cancelled {
                self.publish_job_status(job.id).await;
            }
        }
        let project_name = self
            .projects
            .get_by_id(build.project_id)
            .await?
            .map(|p| p.name)
            .unwrap_or_default();
        self.engine.bus().publish(Event::BuildStatus {
            build_id,
            project_name,
            pipeline_name: build.pipeline_name.clone(),
            number: build.number,
            status: BuildStatus::Cancelled,
            attempt: build.attempt,
        });
        self.match_pass(now).await
    }

    /// fail-fast 级联 / 取消的 CancelBuild 补发：构建下已置 cancelled 且在途
    /// （agent_id 非空）的任务重发取消指令（离线者挂起，重连对账补发）。
    async fn send_pending_cancels_for_build(&self, build_id: i64) -> Result<(), StoreError> {
        for job in self.jobs.channel_cancel_pending_for_build(build_id).await? {
            if let Some(agent_id) = job.agent_id {
                let _ = self.dispatcher.cancel_job(agent_id, build_id, job.id).await;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 事件广播与辅助
    // -----------------------------------------------------------------------

    /// 广播任务当前状态（读回新鲜行；行不存在则跳过——已随级联删除）。
    async fn publish_job_status(&self, job_id: i64) {
        let Ok(Some(job)) = self.jobs.get(job_id).await else {
            return;
        };
        self.engine.bus().publish(Event::JobStatus {
            job_id: job.id,
            build_id: job.build_id,
            stage_index: job.stage_index,
            name: job.name.clone(),
            status: job.status,
            attempt: job.attempt,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{StartBuildInput, TriggerDetail};
    use crate::store::builds::TriggerSource;
    use crate::store::pipelines::PipelineRepo;
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};
    use sisyphus_model::pipeline::{Job, Pipeline, Shell, Stage, Step};
    use std::sync::atomic::{AtomicI64, Ordering};

    /// 内存 fake 下发端口：记录下发/取消；`online=false` 模拟「Agent 无会话」
    /// （dispatch 返回 false → 调度侧回收槽位、回池）。
    #[derive(Default)]
    struct FakeDispatcher {
        dispatched: std::sync::atomic::AtomicU64,
        cancels: std::sync::atomic::AtomicU64,
        online: std::sync::atomic::AtomicBool,
    }

    #[tonic::async_trait]
    impl JobDispatcher for FakeDispatcher {
        async fn dispatch_job(&self, _agent_id: i64, _job: &JobRow) -> Result<bool, SchedError> {
            if !self.online.load(Ordering::SeqCst) {
                return Ok(false);
            }
            self.dispatched.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
        async fn cancel_job(
            &self,
            _agent_id: i64,
            _build_id: i64,
            _job_id: i64,
        ) -> Result<(), SchedError> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 测试装配：真实 store + engine + 单任务 pipeline + 单调时钟（事件驱动
    /// 断言时间语义，不依赖真实 sleep）。
    struct T {
        _dir: tempfile::TempDir,
        pool: sqlx::SqlitePool,
        sched: Scheduler,
        dispatcher: Arc<FakeDispatcher>,
        clock: AtomicI64,
    }

    impl T {
        fn tick_ms(&self, ms: i64) {
            self.clock.fetch_add(ms, Ordering::SeqCst);
        }
        fn now(&self) -> i64 {
            self.clock.load(Ordering::SeqCst)
        }
    }

    /// 装配：建项目 + 存单任务 pipeline（labels 指定）+ 建 Agent（系统标签
    /// 匹配；`agent_labels` 为空则无 Agent）。
    async fn fixture(job_labels: Vec<&str>, agent_labels: Vec<&str>) -> T {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        let pipeline = Pipeline {
            name: "release".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: job_labels.into_iter().map(str::to_string).collect(),
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 30,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "echo hi".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        };
        PipelineRepo::new(pool.clone())
            .save("demo", "release", &pipeline, "tester")
            .await
            .expect("存定义");

        let dispatcher = Arc::new(FakeDispatcher::default());
        dispatcher.online.store(true, Ordering::SeqCst);
        let engine = crate::engine::Engine::new(
            pool.clone(),
            crate::secrets::MasterKey::generate(),
            EventBus::new(),
        );
        let sched = Scheduler::new(engine, pool.clone(), dispatcher.clone(), 10);

        if !agent_labels.is_empty() {
            let agent_json = serde_json::to_string(&agent_labels).expect("json");
            let agents = crate::store::agents::AgentRepo::new(pool.clone());
            let row = agents
                .create(crate::store::agents::NewAgent {
                    name: "linux-1".into(),
                    token_hash: "sisa-hash-sched-test".into(),
                    system_labels: agent_json.clone(),
                    custom_labels: "[]".into(),
                    max_concurrency: 1,
                    register_code_hash: "code-hash".into(),
                })
                .await
                .expect("建 Agent");
            agents
                .mark_online(row.id, &agent_json, None, 1_000_000)
                .await
                .expect("上线");
        }

        T {
            _dir: dir,
            pool,
            sched,
            dispatcher,
            clock: AtomicI64::new(1_000_000),
        }
    }

    /// 触发一条构建（手动）→ 返回构建行。
    async fn start_build(t: &T) -> crate::store::builds::BuildRow {
        t.sched
            .engine
            .start_build(StartBuildInput {
                project_name: "demo".into(),
                pipeline_name: "release".into(),
                trigger: TriggerSource::Manual,
                detail: TriggerDetail {
                    by: "alice".into(),
                    branch: Some("main".into()),
                    commit: None,
                    revision: None,
                    params: vec![],
                },
            })
            .await
            .expect("触发")
    }

    /// AC：匹配语义——有 Agent 命中即下发（dispatch 记数、槽位占）；无匹配
    /// 无限等待 + 标注缺失标签。
    #[tokio::test]
    async fn match_dispatches_to_matching_agent_and_waits_with_missing_tag() {
        let t = fixture(vec!["sisyphus/os=linux"], vec!["sisyphus/os=linux"]).await;
        let build = start_build(&t).await;
        t.sched.engine.drive(build.id).await.expect("推进");
        t.sched.match_pass(t.now()).await.expect("匹配");
        assert_eq!(
            t.dispatcher.dispatched.load(Ordering::SeqCst),
            1,
            "命中即下发"
        );
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(
            jobs[0].status,
            JobStatus::Running,
            "dispatch 已占槽（running）"
        );

        // 无匹配：缺 gpu 标签 → 无限等待 + 标注缺失标签。
        let t2 = fixture(
            vec!["sisyphus/os=linux", "gpu=nvidia"],
            vec!["sisyphus/os=linux"],
        )
        .await;
        let build2 = start_build(&t2).await;
        t2.sched.engine.drive(build2.id).await.expect("推进");
        t2.sched.match_pass(t2.now()).await.expect("匹配");
        let jobs = JobRepo::new(t2.pool.clone())
            .list_by_build(build2.id)
            .await
            .expect("清单");
        assert_eq!(
            jobs[0].status,
            JobStatus::Queued,
            "无匹配保持 queued（无限等待）"
        );
        let reason = jobs[0].waiting_detail.as_deref().expect("等待原因");
        assert!(reason.contains("gpu=nvidia"), "缺失标签标注：{reason}");
    }

    /// AC：job 超时（分钟，0=无限）从下发计时，超时走取消路径终态 timeout。
    #[tokio::test]
    async fn job_timeout_marks_terminal_timeout_after_elapsed() {
        let t = fixture(vec![], vec![]).await;
        let build = start_build(&t).await;
        t.sched.engine.drive(build.id).await.expect("推进");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        JobRepo::new(t.pool.clone())
            .transition(jobs[0].id, JobStatus::Running, None, None, t.now())
            .await
            .expect("置 running");

        // 未到点（timeout_minutes=30）：20 分钟不超时。
        t.tick_ms(20 * 60_000);
        t.sched.timeout_pass(t.now()).await.expect("扫描");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Running, "20 分钟未到点");

        // 到点（累计 31 分钟）：timeout 终态。
        t.tick_ms(11 * 60_000);
        t.sched.timeout_pass(t.now()).await.expect("扫描");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Timeout, "超时走取消路径终态");
    }

    /// AC：orphan 宽限（默认 10 分钟）——unknown 超宽限判 failed。
    #[tokio::test]
    async fn orphan_grace_timeout_marks_failed() {
        let t = fixture(vec![], vec!["x=y"]).await;
        let build = start_build(&t).await;
        t.sched.engine.drive(build.id).await.expect("推进");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        let job_id = jobs[0].id;
        // 建 Agent 并下发（agent_offline_to_unknown 按 agent_id 匹配）。
        let agents = crate::store::agents::AgentRepo::new(t.pool.clone());
        let agent = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-2".into(),
                token_hash: "sisa-hash-sched-orphan".into(),
                system_labels: "[\"x=y\"]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-orphan".into(),
            })
            .await
            .expect("建 Agent");
        agents
            .mark_online(agent.id, "[\"x=y\"]", None, t.now())
            .await
            .expect("上线");
        JobRepo::new(t.pool.clone())
            .dispatch(job_id, agent.id, t.now())
            .await
            .expect("下发");
        // Agent 判离线：running → unknown（unknown_at = 现在）。
        JobRepo::new(t.pool.clone())
            .agent_offline_to_unknown(agent.id, t.now())
            .await
            .expect("转 unknown");

        // 宽限内（9 分钟）：不判败。
        t.tick_ms(9 * 60_000);
        t.sched.orphan_pass(t.now()).await.expect("宽限扫描");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Unknown, "宽限内保持 unknown");

        // 超宽限（累计 10 分钟）：判 failed。
        t.tick_ms(60_000);
        t.sched.orphan_pass(t.now()).await.expect("宽限扫描");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Failed, "宽限超时判失败");
    }

    /// AC：取消（build 级）——运行中下发 CancelBuild（fake dispatcher 记数）；
    /// 排队中移出 pending 池。
    #[tokio::test]
    async fn cancel_build_dispatches_cancel_and_removes_queued() {
        // 有 Agent（fixture 建了一个匹配 x=y 的 Agent）→ 下发成功。
        let t = fixture(vec![], vec!["x=y"]).await;
        let build = start_build(&t).await;
        t.sched.engine.drive(build.id).await.expect("推进");
        t.sched.match_pass(t.now()).await.expect("匹配");
        assert_eq!(t.dispatcher.dispatched.load(Ordering::SeqCst), 1, "已下发");

        // 运行中取消：下发 CancelBuild、构建 cancelled、在途任务行不动（等通道取消）。
        t.sched.cancel_build(build.id).await.expect("取消");
        assert_eq!(
            t.dispatcher.cancels.load(Ordering::SeqCst),
            1,
            "CancelBuild 下发"
        );
        let build_row = crate::store::builds::BuildRepo::new(t.pool.clone())
            .get(build.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build_row.status, BuildStatus::Cancelled, "构建 cancelled");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Running, "在途任务等通道取消");
    }

    /// AC：JobReported 对账——重连上报在跑任务回归 running（清宽限计时）；
    /// Agent 重启丢任务（未上报 + 原 unknown）→ aborted 判失败。
    #[tokio::test]
    async fn job_reported_reconciles_offline_regression_and_aborted_loss() {
        let t = fixture(vec![], vec![]).await;
        let build = start_build(&t).await;
        t.sched.engine.drive(build.id).await.expect("推进");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        let job_id = jobs[0].id;
        // 建 Agent 并下发（by_agent 查询依赖 agent_id）。
        let agents = crate::store::agents::AgentRepo::new(t.pool.clone());
        let agent = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-2".into(),
                token_hash: "sisa-hash-sched-recon".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-recon".into(),
            })
            .await
            .expect("建 Agent");
        agents
            .mark_online(agent.id, "[]", None, t.now())
            .await
            .expect("上线");
        JobRepo::new(t.pool.clone())
            .dispatch(job_id, agent.id, t.now())
            .await
            .expect("下发到 agent");
        // Agent 判离线：running → unknown。
        JobRepo::new(t.pool.clone())
            .agent_offline_to_unknown(agent.id, t.now())
            .await
            .expect("转 unknown");

        // 重连上报在跑：回归 running（清宽限计时）。
        t.sched
            .on_job_reported(agent.id, &[job_id.to_string()], t.now())
            .await
            .expect("对账");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs[0].status, JobStatus::Running, "重连回归 running");
        assert_eq!(jobs[0].unknown_at, None, "重连回归清宽限计时");

        // 重启丢任务：不重连（空上报）+ unknown → aborted 判失败。
        JobRepo::new(t.pool.clone())
            .agent_offline_to_unknown(agent.id, t.now())
            .await
            .expect("再转 unknown");
        t.sched
            .on_job_reported(agent.id, &[], t.now())
            .await
            .expect("对账");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(
            jobs[0].status,
            JobStatus::Aborted,
            "Agent 重启丢任务 aborted"
        );
    }

    /// AC：启动从库重建——非终态构建 drive 放行 + 阶段任务组装（spec 落库）。
    #[tokio::test]
    async fn reconstruct_drives_non_terminal_builds_and_assembles_spec() {
        let t = fixture(vec![], vec![]).await;
        let build = start_build(&t).await;
        let build_row = crate::store::builds::BuildRepo::new(t.pool.clone())
            .get(build.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build_row.status, BuildStatus::Queued, "触发后排队");

        // 重建：drive 放行 + 组装（阶段任务入池）。
        t.sched.reconstruct().await.expect("重建");
        let jobs = JobRepo::new(t.pool.clone())
            .list_by_build(build.id)
            .await
            .expect("清单");
        assert_eq!(jobs.len(), 1, "重建后阶段任务已组装");
        assert_eq!(jobs[0].status, JobStatus::Queued);
        assert!(jobs[0].spec_json.is_some(), "spec 快照已组装");
        let build_row = crate::store::builds::BuildRepo::new(t.pool.clone())
            .get(build.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build_row.status, BuildStatus::Running, "重建放行排队构建");
    }
}

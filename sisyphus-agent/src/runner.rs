//! runner：JobSpec → ack → 步骤序贯执行 → 终态上报（ADR-0002/0006/0008/
//! 0013/0015/0018；票 B3-T5 / #59、B3-T7 / #53）。
//!
//! host + 容器后端执行主体（ADR-0018：两后端只差「进程怎么起」，步骤编排共用
//! [`run_steps`]，后端经 [`Backend`] 在 spawn 缝上分叉）。JobSpec / Cancel 经
//! channel reader 投递到本模块通道：
//!
//! - **ack（占槽位）**：收 JobSpec 即回 [`JobAck`]；内存级同 job 去重（已在跑
//!   → 拒收）。去重与在途上报共用组合根的 `in_flight` 集（[`Handle`] 持其
//!   `Arc`，与 `run_connection` 的 [`crate::channel`] 同源——重连即据此发
//!   `JobReported`），单一在途真源，避免与 [`crate::workspace::RunningJobs`]
//!   并存两套集合发散。
//! - **running**：步骤执行前上报 [`JobStatus`](running)。
//! - **执行后端**：host 直跑（默认）或容器。容器任务由 [`crate::container::ContainerTask`]
//!   装配 per-task 上下文（临时 env 文件 + ASKPASS 挂载 + 容器用户），首步前显式
//!   `docker pull`（always），每步一个一次性 `docker run --rm`（[`Backend::spawn_shell`]
//!   / [`Backend::run_checkout`]）；host 后端经 [`crate::exec`] 起 tokio Command。
//! - **步骤序贯**：每步骤 `StepEvent`（start 含命令回显 / end 含退出码）；
//!   stdout/stderr 合流带 stream 标记、per-job 按 attempt 单调 seq（经
//!   [`crate::logbuf`] 编号）；机密输出字面量脱敏（[`crate::redact`]，跨输出块
//!   边界）；超 `log_limit_bytes` 截断插 `Truncated` 标记不判败。checkout 子命令
//!   经 [`crate::checkout::run_planned`] 共享循环（host/容器只换 spawner）。
//! - **`${SISY_WORKSPACE}`**：任何 shell 步骤执行前替换——host = job 工作区宿主
//!   绝对路径、container = 容器内 `/sisyphus/workspace`
//!   （[`crate::workspace::expand_sisy_workspace`]）。
//! - **取消/超时**：[`crate::exec::kill_tree`] 进程树终止（含子进程）；终态
//!   cancelled / timeout。容器后端额外按名 `docker rm -f` 补刀（幂等，
//!   [`Backend::cleanup_container`]）。取消是电平触发（`watch`）：步骤间到达
//!   亦在下一步 `wait_until` 立即生效。
//! - **终态**：[`JobStatus`](succeeded/failed/cancelled/timeout) + exit_code/
//!   detail；从在途集释放。**离线时完成的终态**缓冲到 [`RunnerUplink`]，重连
//!   经 `flush_pending` 补发（< orphan 宽限窗口；超宽限由 Server orphan grace
//!   兜底判 failed，ADR-0008）。
//! - **checkout 步骤**：交 [`crate::checkout`]（B3-T6 / #60）；容器任务在容器内
//!   执行（镜像须带 git/svn，B3-T7 / #53）。
//! - **产物上传/下载**：本批不做（仅留时序钩子位，票 #59 范围边界）。
//!
//! 上行纪律：JobAck / JobStatus(running) / 终态经 [`RunnerUplink`] 的活体发送器
//! （`run_connection` 每连接 `set_live` 注入，与 logbuf / workspace 同款单 writer
//! 保写序）。终态离线缓冲见上；running 离线丢失由重连 JobReported 回归 running
//! 兜底（ADR-0008）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sisyphus_proto::agent::{
    ChannelMessage, CheckoutStep, JobAck, JobPhase, JobSpec, JobStatus, JobStep, ScmCredential,
    channel_message::Kind, execution_env::Kind as EnvKind, job_step::Kind as StepKind,
};
use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tokio::task::JoinSet;

use crate::ReceiptLog;
use crate::cache::{Cache, RestoreError};
use crate::checkout;
use crate::container;
use crate::exec::{self, SpawnError, StepOutcome};
use crate::logbuf::LogBuffer;
use crate::stepio::{Truncation, emit_step, run_streamed_step, step_event};
use crate::workspace::{self, Workspace};

/// per-job 日志上限默认值（ADR-0013：`log_limit_bytes = 0` → 50 MB）。
const DEFAULT_LOG_LIMIT: u64 = 50 * 1024 * 1024;

// ============================================================
// 上行链路（JobAck / JobStatus 活体发送 + 离线终态缓冲）
// ============================================================

/// runner 上行链路：活体发送器 + 离线终态缓冲。`Clone`——组合根、
/// `run_connection`（set_live / flush_pending）、runner Handle 各持一份共享内部。
///
/// 与 logbuf / workspace 的 `set_live` 同款：`run_connection` 每连接注入
/// `out_tx`；JobAck / JobStatus(running) / 终态经此单 writer 外送（保写序）。
/// 终态遇离线（无活体）缓冲到 `pending_terminals`，重连 `flush_pending` 补发。
#[derive(Clone, Default)]
pub struct RunnerUplink {
    live: Arc<RwLock<Option<mpsc::Sender<ChannelMessage>>>>,
    pending_terminals: Arc<Mutex<HashMap<String, JobStatus>>>,
}

impl RunnerUplink {
    /// 新建（无活体、无缓冲）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入/清除活体发送器（`run_connection` 每连接调用）。`set_live(Some)` 须
    /// 先于 `flush_pending`：缓冲终态经此 sender 补发。
    pub async fn set_live(&self, tx: Option<mpsc::Sender<ChannelMessage>>) {
        *self.live.write().await = tx;
    }

    /// 经活体发送器上行一帧（ack / running）。离线（无活体）则丢弃——ack 恒在
    /// 收到 JobSpec 的活体连接上发，running 离线丢失由重连 JobReported 兜底。
    async fn send(&self, msg: ChannelMessage) {
        let live = self.live.read().await;
        if let Some(tx) = live.as_ref() {
            let _ = tx.send(msg).await;
        }
    }

    /// 上报终态：在线即发；离线则缓冲到 `pending_terminals`，重连 `flush_pending`
    /// 补发。同一 job 重复终态以最后一次为准（HashMap 覆盖）。
    async fn report_terminal(
        &self,
        job_id: &str,
        phase: JobPhase,
        exit_code: Option<i32>,
        detail: &str,
    ) {
        let status = JobStatus {
            job_id: job_id.to_string(),
            phase: phase as i32,
            exit_code,
            detail: detail.to_string(),
        };
        let live = self.live.read().await;
        match live.as_ref() {
            Some(tx) => {
                let _ = tx
                    .send(ChannelMessage {
                        kind: Some(Kind::JobStatus(status)),
                    })
                    .await;
            }
            None => {
                drop(live);
                self.pending_terminals
                    .lock()
                    .await
                    .insert(job_id.to_string(), status);
            }
        }
    }

    /// 重连后补发离线期间缓冲的终态（`run_connection` 在 `set_live(Some)` 之后
    /// 调用）。逐帧经 `out_tx` 外送，与日志重放同一 writer 保写序。清空缓冲。
    pub async fn flush_pending(&self, tx: &mpsc::Sender<ChannelMessage>) {
        let pending = std::mem::take(&mut *self.pending_terminals.lock().await);
        for (_, status) in pending {
            let _ = tx
                .send(ChannelMessage {
                    kind: Some(Kind::JobStatus(status)),
                })
                .await;
        }
    }
}

// ============================================================
// 取消注册表（per-job 电平信号）
// ============================================================

/// per-job 取消信号注册表：job_id → `watch::Sender<bool>`。`Clone` 共享。
/// `cancel`（移除 + 置位）即时触发对应 job 的步骤 `wait_until`；job 完成后
/// `unregister` 清理（取消竞态由 `watch` 电平兜底，移除先后不影响正确性）。
#[derive(Clone, Default)]
struct CancelRegistry {
    inner: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
}

impl CancelRegistry {
    /// 注册一个 job 的取消信号，返回接收端（job 任务持）。已存在则覆盖（不应
    /// 发生——同 job 去重在 ack 前已拒收）。
    async fn register(&self, job_id: &str) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        self.inner.lock().await.insert(job_id.to_string(), tx);
        rx
    }

    /// 取消一个 job（移除其发送器并置位）。返回是否命中（命中即已发信号）。
    async fn cancel(&self, job_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .remove(job_id)
            .is_some_and(|tx| tx.send(true).is_ok())
    }

    /// 清理一个 job 的取消信号（job 任务完成时；已取消则 no-op）。
    async fn unregister(&self, job_id: &str) {
        self.inner.lock().await.remove(job_id);
    }
}

// ============================================================
// 句柄（下行分派循环 owner）
// ============================================================

/// runner 句柄：下行接收端、上行链路、在途集、工作区、日志缓冲、取消注册、
/// 在跑 job 的 [`JoinSet`] 与收帧观测。`run` 消费 self；job 任务经 JoinSet
/// 托管（Handle drop / 任务 abort 即随 JoinSet drop 一并取消）。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    uplink: RunnerUplink,
    in_flight: Arc<RwLock<Vec<String>>>,
    workspace: Workspace,
    cache: Cache,
    logbuf: LogBuffer,
    cancels: CancelRegistry,
    jobs: JoinSet<()>,
    receipts: ReceiptLog,
}

impl Handle {
    /// 以分派接收端、上行链路、在途集、工作区、缓存、日志缓冲与收帧观测构造。
    pub fn new(
        rx: mpsc::Receiver<ChannelMessage>,
        uplink: RunnerUplink,
        in_flight: Arc<RwLock<Vec<String>>>,
        workspace: Workspace,
        cache: Cache,
        logbuf: LogBuffer,
        receipts: ReceiptLog,
    ) -> Self {
        Self {
            rx,
            uplink,
            in_flight,
            workspace,
            cache,
            logbuf,
            cancels: CancelRegistry::default(),
            jobs: JoinSet::new(),
            receipts,
        }
    }

    /// 下行循环：JobSpec → 起任务执行；Cancel → 触发对应 job 取消；同时回收
    /// 已完成的 job 任务（JoinSet reap，防内存累积）。rx 关闭且无在跑 job 即退出。
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                // 下行帧优先：有指令即处理。
                Some(msg) = self.rx.recv() => {
                    match msg.kind {
                        Some(Kind::JobSpec(spec)) => {
                            self.receipts.lock().expect("观测锁").push("job_spec".into());
                            self.handle_job(*spec).await;
                        }
                        Some(Kind::Cancel(cancel)) => {
                            self.receipts.lock().expect("观测锁").push("cancel".into());
                            self.cancels.cancel(&cancel.job_id).await;
                        }
                        _ => {
                            self.receipts.lock().expect("观测锁").push("other".into());
                        }
                    }
                }
                // 回收已完成的 job 任务（释放 JoinSet 条目）。
                Some(res) = self.jobs.join_next() => {
                    if let Err(e) = res {
                        tracing::warn!(error = %e, "job 任务 join 失败");
                    }
                }
                else => break, // rx 关闭且无在跑 job
            }
        }
    }

    /// 处理一帧 JobSpec：去重 → ack → 起任务执行。
    async fn handle_job(&mut self, spec: JobSpec) {
        let job_id = spec.job_id.clone();
        // 去重：已在跑 → 拒收（不占槽位）。
        {
            let mut inflight = self.in_flight.write().await;
            if inflight.iter().any(|j| j == &job_id) {
                self.uplink
                    .send(ChannelMessage {
                        kind: Some(Kind::JobAck(JobAck {
                            job_id: job_id.clone(),
                            accepted: false,
                            error: "job already running".into(),
                        })),
                    })
                    .await;
                return;
            }
            inflight.push(job_id.clone());
        }
        // 接受：ack 占槽位。
        self.uplink
            .send(ChannelMessage {
                kind: Some(Kind::JobAck(JobAck {
                    job_id: job_id.clone(),
                    accepted: true,
                    error: String::new(),
                })),
            })
            .await;

        // 注册取消信号 + 起任务（JoinSet 托管）。
        let cancel_rx = self.cancels.register(&job_id).await;
        let uplink = self.uplink.clone();
        let in_flight = self.in_flight.clone();
        let workspace = self.workspace.clone();
        let cache = self.cache.clone();
        let logbuf = self.logbuf.clone();
        let cancels = self.cancels.clone();
        let job_id_for_cleanup = job_id.clone();
        self.jobs.spawn(async move {
            run_job(spec, cancel_rx, uplink, in_flight, workspace, cache, logbuf).await;
            // 清理取消注册（已取消则 no-op；防 stale 发送器泄漏）。
            cancels.unregister(&job_id_for_cleanup).await;
        });
    }
}

// ============================================================
// job 执行
// ============================================================

/// job 终态（run_steps 产出，映射到 JobPhase 上报；checkout::run 同形返回，
/// 票 B3-T6 / #60）。`pub(crate)`：runner 与 checkout 共用，不外暴露。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobOutcome {
    Succeeded,
    /// 步骤非零退出（携带退出码 + detail）。
    Failed(i32, String),
    Cancelled,
    Timeout,
    /// 步骤 spawn 失败 / 缺二进制 / 规划错（非零退出语义，detail 点名）。
    SpawnFailed(String),
}

// ============================================================
// 执行后端（host / 容器；ADR-0018：两后端只差「进程怎么起」）
// ============================================================

/// 执行后端：host 直跑（默认）或容器。步骤编排（emit start/end、per-job 截断、
/// 机密脱敏、取消/超时竞争、终态映射）共用 [`run_steps`]；后端在此缝上分叉——
/// `${SISY_WORKSPACE}` 展开路径、shell 步骤 spawn、checkout 执行、取消/超时容器补刀。
/// 容器后端持有 per-task [`container::ContainerTask`]（env 文件 + ASKPASS 上下文）。
enum Backend<'a> {
    /// 宿主机直跑。
    Host,
    /// 容器后端（per-task 上下文引用）。
    Container(&'a container::ContainerTask),
}

impl<'a> Backend<'a> {
    /// `${SISY_WORKSPACE}` 展开目标路径：host = 宿主工作区路径；container = 容器内
    /// `/sisyphus/workspace`（挂载点固定，ADR-0011/0018）。
    fn expand_root(&self, ws_dir: &Path) -> PathBuf {
        match self {
            Backend::Host => ws_dir.to_path_buf(),
            Backend::Container(_) => PathBuf::from(container::WORKSPACE_MOUNT_TARGET),
        }
    }

    /// 起一个 shell 步骤进程。host = [`exec::spawn_shell`]（cwd = 工作区、env 注入）；
    /// container = `docker run ... /bin/sh -c <command>`（env 经 env 文件、cwd 经
    /// `-w /sisyphus/workspace`）。返回 (进程句柄, 可选容器名——取消/超时补刀用)。
    fn spawn_shell(
        &self,
        command: &str,
        ws_dir: &Path,
        spec_env: &HashMap<String, String>,
        step_seq: i32,
    ) -> Result<(exec::SpawnedStep, Option<String>), SpawnError> {
        match self {
            Backend::Host => {
                let s = exec::spawn_shell(command, ws_dir, spec_env)?;
                Ok((s, None))
            }
            Backend::Container(task) => {
                let (s, name) = task.spawn_shell(command, step_seq)?;
                Ok((s, Some(name)))
            }
        }
    }

    /// 执行一个 checkout 步骤。host = [`checkout::run`]（宿主侧 git/svn + ASKPASS
    /// 凭据递送）；container = 容器内 checkout（复用 `checkout::plan` +
    /// `checkout::run_planned` + 容器 spawner，凭据经 env 文件 + ASKPASS 挂载）。
    /// 返回 (终态, 可选容器名——取消/超时补刀用)。
    //
    // 参数多于 clippy 阈值：step/ws/credential 是 checkout 输入、余下是 step 上下文
    // ——与 [`checkout::run`] 同款 allow。
    #[allow(clippy::too_many_arguments)]
    async fn run_checkout(
        &self,
        step: &CheckoutStep,
        ws_dir: &Path,
        credential: Option<&ScmCredential>,
        secrets: Vec<Vec<u8>>,
        trunc: Arc<Truncation>,
        job_id: &str,
        attempt: i32,
        cancel_rx: watch::Receiver<bool>,
        deadline: Option<Instant>,
        logbuf: &LogBuffer,
        step_seq: i32,
    ) -> (JobOutcome, Option<String>) {
        match self {
            Backend::Host => {
                let outcome = checkout::run(
                    step,
                    ws_dir,
                    credential,
                    secrets,
                    trunc,
                    job_id,
                    attempt,
                    cancel_rx,
                    deadline,
                    logbuf,
                    &checkout::ScmBins::default(),
                )
                .await;
                (outcome, None)
            }
            Backend::Container(task) => {
                task.run_checkout(
                    step, ws_dir, credential, secrets, trunc, job_id, attempt, cancel_rx, deadline,
                    logbuf, step_seq,
                )
                .await
            }
        }
    }

    /// 取消/超时后补刀（ADR-0018）：host = no-op；container = `docker rm -f <name>`
    /// （幂等——`--rm` 已清则 No such container 报错忽略）。`name = None`（host）no-op。
    async fn cleanup_container(&self, name: Option<String>) {
        if let (Backend::Container(task), Some(name)) = (self, name) {
            task.rm_f(&name).await;
        }
    }

    /// 容器步骤非零退出时的 detail 增补（ADR-0018「镜像缺 sh/所需二进制 = 清晰
    /// 报错」，与 host 后端「缺 X 二进制」哲学对齐）：退出码 127（command not
    /// found）提示镜像可能缺 `sh` 或 `git`/`svn` 等二进制——容器内缺二进制无法像
    /// host 那样在 spawn 时判（二进制在镜像内），退而以 127 退出码兜底给清晰提示。
    /// host 后端 / 非 127 退出原样返回。
    fn augment_failed_detail(&self, code: i32, detail: String) -> String {
        if matches!(self, Backend::Container(_)) && code == 127 {
            format!("{detail}（容器内未找到命令——镜像可能缺 sh 或 git/svn 等二进制）")
        } else {
            detail
        }
    }
}

/// 单个 job 的执行主体：running 上报 → 工作区解析 → 执行后端装配 → 步骤序贯 →
/// 终态上报 → 在途释放。
async fn run_job(
    spec: JobSpec,
    cancel_rx: watch::Receiver<bool>,
    uplink: RunnerUplink,
    in_flight: Arc<RwLock<Vec<String>>>,
    workspace: Workspace,
    cache: Cache,
    logbuf: LogBuffer,
) {
    let job_id = spec.job_id.clone();
    let attempt = spec.attempt;

    // running 上报（离线丢失由重连 JobReported 兜底）。
    uplink
        .send(ChannelMessage {
            kind: Some(Kind::JobStatus(JobStatus {
                job_id: job_id.clone(),
                phase: JobPhase::JobRunning as i32,
                exit_code: None,
                detail: String::new(),
            })),
        })
        .await;

    // 工作区解析失败 → 任务直接失败（无步骤可跑）。
    let ws_dir = match workspace.resolve(&spec.pipeline_name, &spec.job_name) {
        Ok(p) => p,
        Err(e) => {
            uplink
                .report_terminal(
                    &job_id,
                    JobPhase::JobFailed,
                    None,
                    &format!("工作区解析失败：{e}"),
                )
                .await;
            release_inflight(&in_flight, &job_id).await;
            return;
        }
    };

    let secret_values = collect_secrets(&spec);
    let trunc = Arc::new(Truncation::new(log_limit_bytes(spec.log_limit_bytes)));
    let deadline = job_deadline(spec.timeout_minutes);

    // 执行后端（ADR-0018）：容器任务装配 per-task 上下文（env 文件 + ASKPASS；
    // pull 在 run_steps 首步前）；装配失败（image 空 / 写盘失败）→ 任务直接失败。
    // host 直跑无装配。`${SISY_WORKSPACE}` 展开、shell/checkout spawn、取消/超时
    // 补刀经 [`Backend`] 在 run_steps 缝上分叉。`container_task` 持有所有权到
    // run_steps 结束（Drop 删 env 文件 + ASKPASS），`backend` 借用之。
    let container_task = match spec.exec_env.as_ref().and_then(|e| e.kind.as_ref()) {
        Some(EnvKind::Container(c)) => match container::ContainerTask::prepare(c, &spec, &ws_dir) {
            Ok(task) => Some(task),
            Err(e) => {
                uplink
                    .report_terminal(&job_id, JobPhase::JobFailed, None, &e)
                    .await;
                release_inflight(&in_flight, &job_id).await;
                return;
            }
        },
        _ => None,
    };
    let backend = match &container_task {
        Some(task) => Backend::Container(task),
        None => Backend::Host,
    };

    let outcome = run_steps(
        &spec,
        &ws_dir,
        &backend,
        &cache,
        &cancel_rx,
        &secret_values,
        trunc,
        deadline,
        &logbuf,
    )
    .await;

    let (phase, exit_code, detail) = outcome_phase(outcome);
    uplink
        .report_terminal(&job_id, phase, exit_code, &detail)
        .await;
    // 终态上报成功后延迟宽限删除日志缓冲（ADR-0013：宽限内崩溃重启缓冲留作
    // 孤儿补传取证；宽限到期由 logbuf 删除 worker 清理）。
    logbuf.clear_deferred(&job_id, attempt);
    release_inflight(&in_flight, &job_id).await;
    // container_task drop：env 文件 + ASKPASS 任务毕即删（ADR-0018）。
}

/// job 终态 → 上报三元组（phase / exit_code / detail）。纯函数，便于单测覆盖
/// cancelled/timeout/failed/succeeded/spawn-failed 各终态映射（无需真实进程）。
fn outcome_phase(outcome: JobOutcome) -> (JobPhase, Option<i32>, String) {
    match outcome {
        JobOutcome::Succeeded => (JobPhase::JobSucceeded, Some(0), String::new()),
        JobOutcome::Failed(code, d) => (JobPhase::JobFailed, Some(code), d),
        JobOutcome::Cancelled => (JobPhase::JobCancelled, None, "cancelled".into()),
        JobOutcome::Timeout => (JobPhase::JobTimeout, None, "timeout".into()),
        JobOutcome::SpawnFailed(d) => (JobPhase::JobFailed, None, d),
    }
}

/// 步骤序贯执行：shell 步骤起进程、流式编码日志、判退出/取消/超时；checkout
/// 步骤交后端（host = [`checkout::run`]；容器 = 容器内 checkout，B3-T6/T7）。任一
/// 步骤失败/取消/超时即终止并返回对应终态。容器任务首步前显式 `docker pull`
/// （always，ADR-0018）。`trunc` 由调用方 per-job 创建，pull + 全步骤 + 全子命令共享。
///
/// **缓存 restore/save 时机**（ADR-0012）：restore 在最后一个 checkout 步骤后、
/// 其余步骤前（锁文件就位才能算 files 哈希；无 checkout 则首步骤前）；files 缺失 =
/// fail-fast（[`JobOutcome::SpawnFailed`] 点名）。save 仅全步骤成功后（取消/超时/
/// 失败一律不 save——循环内早返回跳过 save）。缓存操作在宿主侧 `ws_dir` 上执行，
/// 容器任务挂载同一工作区、容器内无感知（ADR-0018）。
//
// 参数多于 clippy 阈值：spec/ws/backend/cache 是步骤输入、余下是 step 上下文（取消/脱敏/
// 截断/超时/日志）——与 [`Backend::run_checkout`] 同款 allow。
#[allow(clippy::too_many_arguments)]
async fn run_steps(
    spec: &JobSpec,
    ws_dir: &Path,
    backend: &Backend<'_>,
    cache: &Cache,
    cancel_rx: &watch::Receiver<bool>,
    secret_values: &[Vec<u8>],
    trunc: Arc<Truncation>,
    deadline: Option<Instant>,
    logbuf: &LogBuffer,
) -> JobOutcome {
    // 容器：首步前显式 docker pull（always，ADR-0018）。pull 输出流式进日志；
    // 失败 = 任务失败（detail 含镜像 + 私仓提示）；取消/超时映射对应终态。pull
    // 不创建容器，无需补刀。host 后端跳过。
    if let Backend::Container(task) = backend {
        let pull_outcome = task
            .pull(
                trunc.clone(),
                cancel_rx.clone(),
                deadline,
                logbuf,
                &spec.job_id,
                spec.attempt,
            )
            .await;
        if !matches!(pull_outcome, JobOutcome::Succeeded) {
            return pull_outcome;
        }
    }
    // `${SISY_WORKSPACE}` 展开根：host = 宿主工作区；container = /sisyphus/workspace。
    let expand_root = backend.expand_root(ws_dir);

    // 缓存 restore 时机点（ADR-0012）：最后一个 checkout 步骤后、其余步骤前。
    // restore_before_step = 末个 checkout 的下一索引；无 checkout = 0（首步骤前）。
    // 在该索引的步骤执行前 restore 一次；若该索引 >= 步骤数（末步是 checkout /
    // 无步骤），循环后补做。
    let caches_nonempty = !spec.caches.is_empty();
    let restore_before_step = restore_point(&spec.steps);
    let mut restored = false;

    for (i, step) in spec.steps.iter().enumerate() {
        // restore 时机点命中：本步骤执行前 restore（仅一次）。
        if caches_nonempty && !restored && i == restore_before_step {
            if let Err(detail) = restore_caches(cache, spec, ws_dir).await {
                return JobOutcome::SpawnFailed(detail);
            }
            restored = true;
        }
        let step_seq = step.seq;
        match step.kind.as_ref() {
            Some(StepKind::Shell(shell)) => {
                // `${SISY_WORKSPACE}` 执行前替换（host = 宿主工作区；container =
                // /sisyphus/workspace，ADR-0006/0011/0018）。
                let command = workspace::expand_sisy_workspace(&shell.command, &expand_root);
                let started = now_ms();
                emit_step(
                    logbuf,
                    &spec.job_id,
                    spec.attempt,
                    step_event(step_seq, started, 0, None, &command),
                )
                .await;

                let (spawned, container_name) = match backend
                    .spawn_shell(&command, ws_dir, &spec.env, step_seq)
                {
                    Ok(s) => s,
                    Err(SpawnError(e)) => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, now_ms(), Some(-1), ""),
                        )
                        .await;
                        return JobOutcome::SpawnFailed(format!("步骤 {step_seq} spawn 失败：{e}"));
                    }
                };
                // job 级超时：本步取剩余配额（到点即 0 → 立即 timeout）。流式编码 +
                // wait（取消/超时竞争）+ 回收流任务封在 run_streamed_step（与 checkout
                // 子命令同道）。
                let step_timeout = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
                let outcome = run_streamed_step(
                    spawned,
                    None,
                    secret_values.to_vec(),
                    trunc.clone(),
                    &spec.job_id,
                    spec.attempt,
                    step_timeout,
                    cancel_rx.clone(),
                    logbuf,
                )
                .await;

                let ended = now_ms();
                match outcome {
                    StepOutcome::Exited(code) => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, Some(code), ""),
                        )
                        .await;
                        if code != 0 {
                            return JobOutcome::Failed(
                                code,
                                backend.augment_failed_detail(
                                    code,
                                    format!("步骤 {step_seq} 退出码 {code}"),
                                ),
                            );
                        }
                        // 退出码 0 → 下一步。
                    }
                    StepOutcome::Cancelled => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, None, ""),
                        )
                        .await;
                        backend.cleanup_container(container_name).await;
                        return JobOutcome::Cancelled;
                    }
                    StepOutcome::Timeout => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, None, ""),
                        )
                        .await;
                        backend.cleanup_container(container_name).await;
                        return JobOutcome::Timeout;
                    }
                }
            }
            Some(StepKind::Checkout(checkout)) => {
                // checkout 执行器（B3-T6 / #60）：命令编排 + 凭据递送 + 脱敏链路 +
                // 取消/超时。step start/end 在本层包裹，子命令输出经 stepio 流式编码。
                let started = now_ms();
                // step start 命令回显：脱敏摘要（repo_url + 目标，绝不含凭据）。
                let echo = checkout::step_echo(checkout);
                emit_step(
                    logbuf,
                    &spec.job_id,
                    spec.attempt,
                    step_event(step_seq, started, 0, None, &echo),
                )
                .await;
                let (outcome, container_name) = backend
                    .run_checkout(
                        checkout,
                        ws_dir,
                        spec.scm_credential.as_ref(),
                        secret_values.to_vec(),
                        trunc.clone(),
                        &spec.job_id,
                        spec.attempt,
                        cancel_rx.clone(),
                        deadline,
                        logbuf,
                        step_seq,
                    )
                    .await;
                let ended = now_ms();
                match outcome {
                    JobOutcome::Succeeded => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, Some(0), ""),
                        )
                        .await;
                        // 退出码 0 → 下一步。
                    }
                    JobOutcome::Failed(code, d) => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, Some(code), ""),
                        )
                        .await;
                        return JobOutcome::Failed(
                            code,
                            backend.augment_failed_detail(code, format!("步骤 {step_seq}：{d}")),
                        );
                    }
                    JobOutcome::Cancelled => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, None, ""),
                        )
                        .await;
                        backend.cleanup_container(container_name).await;
                        return JobOutcome::Cancelled;
                    }
                    JobOutcome::Timeout => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, None, ""),
                        )
                        .await;
                        backend.cleanup_container(container_name).await;
                        return JobOutcome::Timeout;
                    }
                    JobOutcome::SpawnFailed(d) => {
                        emit_step(
                            logbuf,
                            &spec.job_id,
                            spec.attempt,
                            step_event(step_seq, started, ended, Some(-1), ""),
                        )
                        .await;
                        return JobOutcome::SpawnFailed(format!("步骤 {step_seq}：{d}"));
                    }
                }
            }
            None => {
                // 无 kind 的步骤（契约演进，不应发生）：跳过。
                tracing::warn!(step_seq, "步骤缺 kind，跳过");
                continue;
            }
        }
    }
    // restore 时机点 >= 步骤数（末步是 checkout / 无步骤）且未 restore：循环后
    // 补做——此时全步骤已成功（循环内失败已早返回）。
    if caches_nonempty
        && !restored
        && let Err(detail) = restore_caches(cache, spec, ws_dir).await
    {
        return JobOutcome::SpawnFailed(detail);
    }
    // save：仅全步骤成功后、先于产物上传（本批无上传传输，即步骤成功后立即 save；
    // ADR-0012）。save 失败已告警不判败——忽略返回。
    if caches_nonempty {
        save_caches(cache, spec, ws_dir).await;
    }
    JobOutcome::Succeeded
}

// ============================================================
// 缓存 restore/save 时机钩子（ADR-0012）
// ============================================================

/// 缓存 restore 时机点（ADR-0012）：末个 checkout 步骤的下一索引；无 checkout
/// = 0（首步骤前）。锁文件就位才能算 files 哈希——故 restore 必在 checkout 之后。
/// 纯函数，便于单测时机正确性（无需真实 checkout 执行）。
pub(crate) fn restore_point(steps: &[JobStep]) -> usize {
    steps
        .iter()
        .rposition(|s| matches!(s.kind, Some(StepKind::Checkout(_))))
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// 在 restore 时机点对本任务所有缓存声明执行 restore。files 缺失 = `Err`（fail-fast，
/// runner 映射 [`JobOutcome::SpawnFailed`] 点名）；restore 拷贝失败已当 miss（不进
/// `Err`，构建照常跑）。无缓存声明 = no-op。pipeline 名取 `spec.pipeline_name`。
async fn restore_caches(cache: &Cache, spec: &JobSpec, ws_dir: &Path) -> Result<(), String> {
    for c in &spec.caches {
        if let Err(RestoreError::MissingFile(f)) =
            cache.restore(&spec.pipeline_name, c, ws_dir).await
        {
            return Err(format!("缓存锁文件缺失：{f}"));
        }
    }
    Ok(())
}

/// 全部步骤成功后 save 各缓存声明。save 失败已告警不判败（ADR-0012）——此处
/// 忽略返回。无缓存声明 = no-op。
async fn save_caches(cache: &Cache, spec: &JobSpec, ws_dir: &Path) {
    for c in &spec.caches {
        cache.save(&spec.pipeline_name, c, ws_dir).await;
    }
}

// ============================================================
// 纯逻辑助手
// ============================================================

/// 收集本任务需脱敏的机密字面量：`secrets` 命名的 env 值 + checkout 凭据
/// password（ADR-0015：注入 env + checkout 凭据，离机前字面量替换）。
/// 空值滤除（空机密无意义）。
fn collect_secrets(spec: &JobSpec) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for name in &spec.secrets {
        if let Some(v) = spec.env.get(name)
            && !v.is_empty()
        {
            out.push(v.as_bytes().to_vec());
        }
    }
    if let Some(cred) = spec.scm_credential.as_ref()
        && !cred.password.is_empty()
    {
        out.push(cred.password.as_bytes().to_vec());
    }
    out
}

/// per-job 日志上限（ADR-0013：`log_limit_bytes <= 0` → 默认 50 MB）。
fn log_limit_bytes(raw: i64) -> u64 {
    if raw <= 0 {
        DEFAULT_LOG_LIMIT
    } else {
        raw as u64
    }
}

/// job 级超时 deadline（ADR-0008：`timeout_minutes <= 0` = 无限；否则 now + N 分钟）。
fn job_deadline(timeout_minutes: i64) -> Option<Instant> {
    if timeout_minutes <= 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(timeout_minutes as u64 * 60))
    }
}

/// 从在途集释放一个 job（终态后）。
async fn release_inflight(in_flight: &Arc<RwLock<Vec<String>>>, job_id: &str) {
    let mut g = in_flight.write().await;
    g.retain(|j| j != job_id);
}

/// Unix 毫秒时间戳（与 workspace.rs 同源；尽力而为：系统时钟异常回退 0）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ============================================================
// 单元测试（纯逻辑助手：collect_secrets / log_limit / deadline / outcome_phase）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::agent::{
        ContainerEnv, ExecutionEnv, JobStep, ScmCredential, ShellStep,
        execution_env::Kind as EnvKind,
    };
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn collect_secrets_from_env_and_scm_password() {
        let mut env = env_map(&[
            ("DEPLOY_KEY", "deploy-val"),
            ("PATH", "/bin"),
            ("EMPTY", ""),
        ]);
        let spec = JobSpec {
            secrets: vec!["DEPLOY_KEY".into(), "MISSING".into(), "EMPTY".into()],
            env: std::mem::take(&mut env),
            scm_credential: Some(ScmCredential {
                username: "u".into(),
                password: "pw-token".into(),
            }),
            ..Default::default()
        };
        let secrets = collect_secrets(&spec);
        assert!(secrets.contains(&b"deploy-val".to_vec()), "env 机密值收录");
        assert!(
            secrets.contains(&b"pw-token".to_vec()),
            "checkout 凭据 password 收录"
        );
        assert!(!secrets.iter().any(|s| s.is_empty()), "空值滤除");
        assert!(!secrets.iter().any(|s| s == b"/bin"), "非机密 env 值不收录");
        assert!(
            !secrets.iter().any(|s| s == b"u"),
            "checkout username 非机密，不收录"
        );
    }

    #[test]
    fn collect_secrets_empty_when_none() {
        let spec = JobSpec {
            secrets: vec![],
            env: env_map(&[("X", "y")]),
            scm_credential: None,
            ..Default::default()
        };
        assert!(collect_secrets(&spec).is_empty());
    }

    #[test]
    fn log_limit_default_when_zero_or_negative() {
        assert_eq!(log_limit_bytes(0), DEFAULT_LOG_LIMIT);
        assert_eq!(log_limit_bytes(-1), DEFAULT_LOG_LIMIT);
        assert_eq!(log_limit_bytes(1024), 1024);
    }

    #[test]
    fn job_deadline_none_when_zero_or_negative() {
        assert!(job_deadline(0).is_none());
        assert!(job_deadline(-5).is_none());
        assert!(job_deadline(1).is_some(), "正分钟 → 有 deadline");
    }

    #[test]
    fn outcome_phase_maps_all_terminals() {
        assert_eq!(
            outcome_phase(JobOutcome::Succeeded),
            (JobPhase::JobSucceeded, Some(0), String::new())
        );
        assert_eq!(
            outcome_phase(JobOutcome::Failed(7, "步骤 1 退出码 7".into())),
            (JobPhase::JobFailed, Some(7), "步骤 1 退出码 7".into())
        );
        assert_eq!(
            outcome_phase(JobOutcome::Cancelled),
            (JobPhase::JobCancelled, None, "cancelled".into())
        );
        assert_eq!(
            outcome_phase(JobOutcome::Timeout),
            (JobPhase::JobTimeout, None, "timeout".into())
        );
        assert_eq!(
            outcome_phase(JobOutcome::SpawnFailed("spawn 失败：x".into())),
            (JobPhase::JobFailed, None, "spawn 失败：x".into())
        );
    }

    /// restore 时机点（ADR-0012）：末个 checkout 的下一索引；无 checkout = 0。
    /// 纯函数——无需真实 checkout 执行即可断言时机正确性。
    #[test]
    fn restore_point_after_last_checkout_or_before_first_step() {
        use sisyphus_proto::agent::{CheckoutStep, ShellStep, VcsType};
        let shell = || JobStep {
            name: "s".into(),
            seq: 0,
            kind: Some(StepKind::Shell(ShellStep { command: String::new() })),
        };
        let checkout = || JobStep {
            name: "c".into(),
            seq: 0,
            kind: Some(StepKind::Checkout(CheckoutStep {
                vcs: VcsType::VcsGit as i32,
                repo_url: String::new(),
                r#ref: String::new(),
                commit: String::new(),
                submodules: false,
            })),
        };
        // 无步骤 → 0。
        assert_eq!(restore_point(&[]), 0);
        // 仅 shell → 0（无 checkout，首步骤前 restore）。
        assert_eq!(restore_point(&[shell()]), 0);
        assert_eq!(restore_point(&[shell(), shell()]), 0);
        // 仅 checkout → 1（末个 checkout 是 idx 0，下一索引 1）。
        assert_eq!(restore_point(&[checkout()]), 1);
        // checkout + shell → 1（restore 在 checkout 后、shell 前）。
        assert_eq!(restore_point(&[checkout(), shell()]), 1);
        // shell + checkout + shell → 2（末个 checkout 在 idx 1，restore 在 idx 2 前）。
        assert_eq!(restore_point(&[shell(), checkout(), shell()]), 2);
        // 多 checkout：取最后一个 → 3。
        assert_eq!(
            restore_point(&[checkout(), shell(), checkout(), shell()]),
            3,
            "末个 checkout 后 restore"
        );
        // 全是 checkout → 末个之后（len）。
        assert_eq!(restore_point(&[checkout(), checkout()]), 2);
    }

    /// AC（ADR-0018「镜像缺 sh/所需二进制 = 清晰报错」）：容器步骤退出 127（command
    /// not found）→ detail 增补「镜像可能缺 sh 或 git/svn」；host 后端 + 非 127 退出
    /// 原样。无需 daemon（纯 Backend 逻辑）。
    #[tokio::test]
    async fn backend_container_exit_127_adds_missing_binary_hint() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let ws = Workspace::new(dir.path().join("workspaces"));
        let ws_dir = ws.resolve("pipe", "job").expect("resolve 工作区");
        let spec = JobSpec {
            job_id: "job-bin".into(),
            pipeline_name: "pipe".into(),
            job_name: "job".into(),
            exec_env: Some(ExecutionEnv {
                kind: Some(EnvKind::Container(ContainerEnv {
                    image: "alpine:3.20".into(),
                })),
            }),
            ..Default::default()
        };
        let c = match spec.exec_env.as_ref().and_then(|e| e.kind.as_ref()) {
            Some(EnvKind::Container(c)) => c,
            _ => panic!("spec 应为容器执行环境"),
        };
        let task =
            container::ContainerTask::prepare(c, &spec, &ws_dir).expect("prepare container task");
        let backend = Backend::Container(&task);

        // 容器退出 127 → 增补缺二进制提示。
        let d127 = backend.augment_failed_detail(127, "步骤 0 退出码 127".into());
        assert!(
            d127.contains("镜像可能缺 sh"),
            "容器 127 应提示镜像缺二进制：{d127}"
        );
        // 容器非 127 退出（如 7）→ 不增补。
        let d7 = backend.augment_failed_detail(7, "步骤 0 退出码 7".into());
        assert!(!d7.contains("镜像可能缺"), "非 127 不增补：{d7}");

        // host 后端退出 127 → 不增补（host 的缺二进制在 spawn 时已清晰报错）。
        let host = Backend::Host;
        let dh = host.augment_failed_detail(127, "步骤 0 退出码 127".into());
        assert!(!dh.contains("镜像可能缺"), "host 不增补容器提示：{dh}");
    }

    /// emit_step / emit_output 的 seq 由 logbuf 编号、流式编码合流由集成测试
    /// 覆盖（需真实进程 + 通道）；此处仅断言纯助手。
    #[test]
    fn shell_command_placeholder_expansion_in_step() {
        // 证实 step start 携带的是已展开命令（runner 职责：执行前替换）。
        let ws = Path::new("/srv/ws/pipe/job");
        let expanded = workspace::expand_sisy_workspace("echo ${SISY_WORKSPACE}/out", ws);
        assert_eq!(expanded, "echo /srv/ws/pipe/job/out");
        let _ = ShellStep {
            command: expanded.clone(),
        };
        assert!(expanded.contains("/srv/ws/pipe/job"));
    }

    /// run_steps 的真实超时路径（AC: 终态 timeout 正确上报）：直接喂一个长睡眠
    /// 步骤 + 近 deadline（300ms，不依赖 timeout_minutes 的整分钟下限——那是
    /// 集成层对 CI 过慢的限制，本测试在编排层以 Option<Instant> 直入），
    /// 断言返回 `JobOutcome::Timeout`。覆盖 deadline → wait_until(Timeout) →
    /// kill_tree → JobTimeout 的完整编排链（进程树终止本身由 exec 单测覆盖）。
    #[tokio::test]
    async fn run_steps_short_deadline_yields_timeout() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let logbuf = LogBuffer::new(dir.path().join("logbuf"), Duration::from_secs(60));
        std::fs::create_dir_all(dir.path().join("logbuf")).expect("建 logbuf 目录");
        let ws = Workspace::new(dir.path().join("workspaces"));
        let ws_dir = ws.resolve("pipe", "job").expect("resolve 工作区");
        let cache = Cache::new(dir.path().join("cache"), 0);

        let command = if cfg!(unix) {
            "sleep 30".to_string()
        } else {
            "ping -n 30 127.0.0.1".to_string()
        };
        let spec = JobSpec {
            job_id: "job-t".into(),
            pipeline_name: "pipe".into(),
            job_name: "job".into(),
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Shell(ShellStep { command })),
            }],
            ..Default::default()
        };
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let deadline = Some(Instant::now() + Duration::from_millis(300));
        let started = std::time::Instant::now();
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run_steps(
            &spec,
            &ws_dir,
            &Backend::Host,
            &cache,
            &cancel_rx,
            &[],
            trunc,
            deadline,
            &logbuf,
        )
        .await;
        assert_eq!(outcome, JobOutcome::Timeout, "近 deadline 应使步骤超时");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时应 promptly 终止进程树（非睡满 30s）"
        );
    }

    /// run_steps 的取消路径（AC: 终态 cancelled）：长睡眠 + 预置取消信号 →
    /// `JobOutcome::Cancelled`，且 promptly（电平触发，进入 wait 即见）。
    #[tokio::test]
    async fn run_steps_pre_set_cancel_yields_cancelled() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let logbuf = LogBuffer::new(dir.path().join("logbuf"), Duration::from_secs(60));
        std::fs::create_dir_all(dir.path().join("logbuf")).expect("建 logbuf 目录");
        let ws = Workspace::new(dir.path().join("workspaces"));
        let ws_dir = ws.resolve("pipe", "job").expect("resolve 工作区");
        let cache = Cache::new(dir.path().join("cache"), 0);

        let command = if cfg!(unix) {
            "sleep 30".to_string()
        } else {
            "ping -n 30 127.0.0.1".to_string()
        };
        let spec = JobSpec {
            job_id: "job-c".into(),
            pipeline_name: "pipe".into(),
            job_name: "job".into(),
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Shell(ShellStep { command })),
            }],
            ..Default::default()
        };
        let (_cancel_tx, cancel_rx) = watch::channel(true); // 预置取消
        let started = std::time::Instant::now();
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run_steps(
            &spec,
            &ws_dir,
            &Backend::Host,
            &cache,
            &cancel_rx,
            &[],
            trunc,
            None,
            &logbuf,
        )
        .await;
        assert_eq!(
            outcome,
            JobOutcome::Cancelled,
            "预置取消应使步骤即取 cancelled"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "预置取消应立即返回"
        );
    }
}

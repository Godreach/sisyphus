//! sisyphus-agent 库面：Agent 全部模块与组合根（ADR-0009）。
//!
//! bin（`src/main.rs`）是薄壳：解析 CLI → 合并配置（ADR-0010）→ 初始化
//! tracing（ADR-0019）→ 经 [`Agent`] 装配组合根 → 常驻运行（断线指数退避
//! 重连，永久重试不自杀）。
//!
//! 集成测试与二进制共用同一组合根（票 B3-T1）：`tests/` 经
//! [`Agent::with_channel_config`] 注入短心跳间隔/短退避与 fake Server 对跑，
//! 不起独立进程（B2c 同纪律）。

pub mod cache;
pub mod channel;
pub mod config;
pub mod exec;
pub mod logbuf;
pub mod redact;
pub mod register;
pub mod runner;
pub mod upgrader;
pub mod workspace;

use std::sync::Arc;

use channel::ChannelConfig;
use tokio::sync::{RwLock, mpsc, watch};
use workspace::Workspace;

use crate::logbuf::{DEFAULT_GRACE, LogBuffer};

/// 占位模块收帧观测：各占位循环收到下行指令即记录其类别（分派骨架的
/// 断言面——「占位 handle 可收」的唯一外部可观测信号；真实执行随各批次
/// 换入后移除）。
pub type ReceiptLog = Arc<std::sync::Mutex<Vec<String>>>;

/// Agent 组合根：配置 + 通道（连接/心跳/重连/分派）+ 日志缓冲 + 各模块占位句柄。
pub struct Agent {
    /// 启动配置（数据目录五处约定的来源）。本批装配后不再读取——
    /// workspace/cache/logbuf 等后续批次从 `data_dir` 派生路径，
    /// 保留在组合根上即它们的位置。
    #[allow(dead_code)]
    config: config::Config,
    channel_cfg: ChannelConfig,
    /// 下行分派（reader → 各模块通道）。
    dispatch: channel::Dispatch,
    /// 在途任务集（runner 维护，重连随 JobReported 上报）。
    in_flight: Arc<RwLock<Vec<String>>>,
    /// 日志 seq 缓冲（ADR-0007/0013：先落盘再发出、断线累计、重连幂等重放、
    /// 终态宽限删除/孤儿补传后删除）。
    logbuf: LogBuffer,
    /// runner 上行链路（JobAck/JobStatus 活体发送 + 离线终态缓冲；`run_connection`
    /// 每连接 set_live / flush_pending）。
    runner_uplink: runner::RunnerUplink,
    /// 工作区共享状态（ADR-0011：根 + 活体上行 + 占用采样源；`run_connection`
    /// 持引用 set_live/读采样）。
    workspace_state: Workspace,
    /// 工作区占用采样器（低频后台遍历；`run` 用 channel_cfg 的间隔 spawn）。
    workspace_sampler: Arc<workspace::WorkspaceSampler>,
    /// 各模块占位句柄（真实执行随后续批次换入）。
    runner: runner::Handle,
    workspace: workspace::Handle,
    cache: cache::Handle,
    upgrader: upgrader::Handle,
    /// 占位模块收帧观测（分派骨架断言面）。
    receipts: ReceiptLog,
}

impl Agent {
    /// 以默认通道参数（15s 心跳、1s/×2/60s/±20% 退避）装配组合根。
    /// token 从数据目录读取（缺 = 无凭据连接，被拒后退避重试）。工作区采样
    /// 用默认 10 分钟间隔（ADR-0011/0019）。
    pub fn new(config: config::Config) -> Self {
        // 工作区共享状态 + 低频采样器（喂心跳 workspace_bytes）。
        let workspace_state = Workspace::new(config.workspaces_dir());
        let workspace_sampler = Arc::new(workspace::WorkspaceSampler::new(config.workspaces_dir()));
        let workspace_state = workspace_state.with_usage(workspace_sampler.clone());
        let channel_cfg = ChannelConfig {
            server_url: config.server_url.clone(),
            token: config::read_token(&config.data_dir),
            heartbeat_interval: channel::HEARTBEAT_INTERVAL,
            backoff: channel::Backoff::new(),
            labels: Arc::new(channel::PlatformLabels),
            disk: Arc::new(channel::PlatformDiskSampler::new(config.data_dir.clone())),
            workspace_sample_interval: workspace::DEFAULT_WORKSPACE_SAMPLE_INTERVAL,
        };
        Self::with_channel_config(config, channel_cfg, workspace_state, workspace_sampler)
    }

    /// 以指定通道参数装配（测试注入短心跳/短退避/固定采样求确定性）。
    /// 日志缓冲用默认宽限（1 分钟，ADR-0013）。工作区采样间隔取自
    /// `channel_cfg.workspace_sample_interval`。
    pub fn with_channel_config(
        config: config::Config,
        channel_cfg: ChannelConfig,
        workspace_state: Workspace,
        workspace_sampler: Arc<workspace::WorkspaceSampler>,
    ) -> Self {
        // 分派通道：各模块持有接收端，reader 持有发送端。
        let (runner_tx, runner_rx) = mpsc::channel(channel::DISPATCH_CAPACITY);
        let (upgrader_tx, upgrader_rx) = mpsc::channel(channel::DISPATCH_CAPACITY);
        let (workspace_tx, workspace_rx) = mpsc::channel(channel::DISPATCH_CAPACITY);
        let (cache_tx, cache_rx) = mpsc::channel(channel::DISPATCH_CAPACITY);
        let dispatch = channel::Dispatch {
            runner: runner_tx,
            upgrader: upgrader_tx,
            workspace: workspace_tx,
            cache: cache_tx,
        };
        let receipts = ReceiptLog::default();
        let logbuf = LogBuffer::new(config.logbuf_dir(), DEFAULT_GRACE);
        let in_flight = Arc::new(RwLock::new(Vec::new()));
        let runner_uplink = runner::RunnerUplink::new();
        // Handle 持一份工作区状态克隆（与组合根共享内部；run_connection 用根上那份）。
        let handle_state = workspace_state.clone();
        Self {
            config,
            channel_cfg,
            dispatch,
            in_flight: in_flight.clone(),
            logbuf: logbuf.clone(),
            runner_uplink: runner_uplink.clone(),
            workspace_state,
            workspace_sampler,
            runner: runner::Handle::new(
                runner_rx,
                runner_uplink,
                in_flight,
                handle_state.clone(),
                logbuf,
                receipts.clone(),
            ),
            workspace: workspace::Handle::new(workspace_rx, handle_state, receipts.clone()),
            cache: cache::Handle::new(cache_rx, receipts.clone()),
            upgrader: upgrader::Handle::new(upgrader_rx, receipts.clone()),
            receipts,
        }
    }

    /// 日志缓冲句柄（runner 喂事件/测试断言）。
    pub fn logbuf(&self) -> LogBuffer {
        self.logbuf.clone()
    }

    /// 占位模块收帧观测（分派骨架断言面；测试/集成测试用）。
    pub fn receipts(&self) -> ReceiptLog {
        self.receipts.clone()
    }

    /// 工作区共享状态（集成测试用它 resolve/清理真实工作区目录、读采样）。
    pub fn workspace_state(&self) -> Workspace {
        self.workspace_state.clone()
    }

    /// 常驻运行：占位模块循环 + 通道重连循环。断线即指数退避重连、永久
    /// 重试不自杀（认证拒绝/版本拒连/网络失败一律重试，日志写明原因）。
    /// `shutdown` 收到 `true` 时退出（测试/将来服务化用；生产主进程常驻）。
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        // 各模块占位循环（收下行指令即记日志；真实执行随后续批次）。
        let runner_task = tokio::spawn(self.runner.run());
        let workspace_task = tokio::spawn(self.workspace.run());
        let cache_task = tokio::spawn(self.cache.run());
        let upgrader_task = tokio::spawn(self.upgrader.run());

        // 工作区占用采样循环（ADR-0011/0019：低频后台遍历，spawn 先采样一次；
        // 间隔取自 channel_cfg——测试注入短间隔避免真实 10 分钟 sleep）。
        let sampler_task = self
            .workspace_sampler
            .clone()
            .spawn(self.channel_cfg.workspace_sample_interval);

        let mut backoff = self.channel_cfg.backoff.clone();
        loop {
            // 连接期间在途上报/心跳由 run_connection 内部驱动；连接结束
            // （对端关流/读失败/认证拒连）即进入退避。连接常驻时间 ≥ 一个
            // 心跳间隔视为「健康会话」，退避复位——掉线重连回到 1s 起步。
            let started = std::time::Instant::now();
            let outcome = tokio::select! {
                r = channel::run_connection(
                    &self.channel_cfg,
                    &self.dispatch,
                    self.in_flight.clone(),
                    &self.logbuf,
                    &self.runner_uplink,
                    &self.workspace_state,
                ) => r,
                _ = shutdown_requested(&mut shutdown) => break,
            };
            match outcome {
                Ok(()) => tracing::info!("通道关闭（对端关流），进入退避重连"),
                Err(e) => tracing::warn!(error = %e, "通道连接结束，进入退避重连"),
            }
            if started.elapsed() >= self.channel_cfg.heartbeat_interval {
                backoff.reset();
            }

            let delay = backoff.next_delay();
            tracing::info!(delay_ms = delay.as_millis() as u64, "退避后重连");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown_requested(&mut shutdown) => break,
            }
        }

        runner_task.abort();
        workspace_task.abort();
        cache_task.abort();
        upgrader_task.abort();
        sampler_task.abort();
    }
}

/// 是否收到关闭信号（watch 值变 true；发送端弃置视为无信号——生产主进程
/// 持发送端，进程生命即运行生命）。
async fn shutdown_requested(rx: &mut watch::Receiver<bool>) -> bool {
    if *rx.borrow() {
        return true;
    }
    match rx.changed().await {
        Ok(_) => *rx.borrow(),
        Err(_) => false,
    }
}

//! sisyphus-agent 库面：Agent 全部模块与组合根（ADR-0009）。
//!
//! bin（`src/main.rs`）是薄壳：解析 CLI → 合并配置（ADR-0010）→ 初始化
//! tracing（ADR-0019）→ 经 [`Agent`] 装配组合根 → 常驻运行（断线指数退避
//! 重连，永久重试不自杀）。
//!
//! 集成测试与二进制共用同一组合根（票 B3-T1）：`tests/` 经
//! [`Agent::with_channel_config`] 注入短心跳间隔/短退避与 fake Server 对跑，
//! 不起独立进程（B2c 同纪律）。

pub mod artifacts;
pub mod cache;
pub mod channel;
pub mod checkout;
pub mod config;
pub mod container;
pub mod exec;
pub mod logbuf;
pub mod redact;
pub mod register;
pub mod runner;
pub mod stepio;
pub mod upgrader;
#[cfg(windows)]
pub mod windows_job;
pub mod workspace;

use std::sync::Arc;

use cache::Cache;
use channel::ChannelConfig;
use tokio::sync::{RwLock, mpsc, watch};
use upgrader::{DrainGate, UpgradeDeps, UpgradeUplink};
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
    /// 缓存共享状态（ADR-0012：根 + 容量 + registry；runner restore/save 时机
    /// 钩子 + `run_connection` set_live/读 cache_bytes）。
    cache_state: Cache,
    /// 各模块占位句柄（真实执行随后续批次换入）。
    runner: runner::Handle,
    workspace: workspace::Handle,
    cache: cache::Handle,
    /// upgrader 下行接收端（`run` 时装配 Handle——延后装配以支持 `with_upgrader_deps`
    /// 在 `run` 前注入 fake 下载/启动器，避免测试真下载/真重启进程）。
    upgrader_rx: mpsc::Receiver<sisyphus_proto::agent::ChannelMessage>,
    /// 升级依赖包（下载器/启动器/当前二进制路径）。默认 `safe_stub`（不触发升级
    /// 的测试即安全）；`Agent::new` 覆盖为 real，测试经 `with_upgrader_deps` 注入 fake。
    upgrade_deps: UpgradeDeps,
    /// 升级上行链路（ADR-0017：升级阶段经通道上报；`run_connection` set_live/flush）。
    upgrade_uplink: UpgradeUplink,
    /// 排空闸门（ADR-0017：升级排空时 runner 拒接新任务；runner 终态释放唤醒）。
    /// runner Handle 持其克隆；upgrader Handle（`run` 装配）持其克隆。
    drain_gate: DrainGate,
    /// 升级成功信号发送端（upgrader 置位 → `run` 循环退出，旧进程退出）。
    exit_tx: watch::Sender<bool>,
    /// 升级成功信号接收端（`run` 循环 select 监听，置位即退出）。
    exit_rx: watch::Receiver<bool>,
    /// 占位模块收帧观测（分派骨架断言面）。
    receipts: ReceiptLog,
    /// 产物传输缝（票 #74）：runner 上传/依赖下载；默认 real（api_url /
    /// token 取配置），测试经 [`Self::with_artifact_io`] 注入 fake。
    artifact_io: Arc<dyn artifacts::ArtifactIo>,
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
        // 缓存共享状态（ADR-0012：缓存根 + 容量上限，registry 载入）。
        let cache_state = Cache::new(config.cache_dir(), config.cache_capacity_bytes());
        let channel_cfg = ChannelConfig {
            server_url: config.server_url.clone(),
            token: config::read_token(&config.data_dir),
            heartbeat_interval: channel::HEARTBEAT_INTERVAL,
            backoff: channel::Backoff::new(),
            labels: Arc::new(channel::PlatformLabels::new(Arc::new(
                channel::ContainerProbe::new(),
            ))),
            disk: Arc::new(channel::PlatformDiskSampler::new(config.data_dir.clone())),
            workspace_sample_interval: workspace::DEFAULT_WORKSPACE_SAMPLE_INTERVAL,
        };
        Self::with_channel_config(
            config,
            channel_cfg,
            workspace_state,
            workspace_sampler,
            cache_state,
        )
        .with_upgrader_deps(UpgradeDeps::real())
    }

    /// 以指定通道参数装配（测试注入短心跳/短退避/固定采样求确定性）。
    /// 日志缓冲用默认宽限（1 分钟，ADR-0013）。工作区采样间隔取自
    /// `channel_cfg.workspace_sample_interval`。
    pub fn with_channel_config(
        config: config::Config,
        channel_cfg: ChannelConfig,
        workspace_state: Workspace,
        workspace_sampler: Arc<workspace::WorkspaceSampler>,
        cache_state: Cache,
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
        let handle_cache = cache_state.clone();
        // 排空闸门（ADR-0017）：runner 拒接新任务 + 终态释放唤醒；upgrader 等在途空。
        // runner Handle 与 upgrader Handle（run 装配）各持克隆共享同一内部。
        let drain_gate = DrainGate::new();
        // 升级上行链路（ADR-0017）：阶段经通道上报；run_connection set_live/flush。
        let upgrade_uplink = UpgradeUplink::new();
        // 升级成功信号：upgrader 置位 → run 循环退出（旧进程退出，新进程接管）。
        let (exit_tx, exit_rx) = watch::channel(false);
        // 升级依赖：默认 safe_stub（不触发升级的测试即安全）；Agent::new / 测试覆盖。
        let upgrade_deps = UpgradeDeps::safe_stub();
        // 产物传输（票 #74）：REST 基址与 token 取配置（api_url 缺失时调用
        // 恒 Unconfigured 明确报错）；测试经 `with_artifact_io` 注入 fake。
        let artifact_io: Arc<dyn artifacts::ArtifactIo> = Arc::new(artifacts::RealArtifactIo::new(
            config.api_url.clone(),
            channel_cfg.token.clone(),
        ));
        Self {
            config,
            channel_cfg,
            dispatch,
            in_flight: in_flight.clone(),
            logbuf: logbuf.clone(),
            runner_uplink: runner_uplink.clone(),
            workspace_state,
            workspace_sampler,
            cache_state,
            runner: runner::Handle::new(
                runner_rx,
                runner_uplink,
                in_flight,
                handle_state.clone(),
                handle_cache.clone(),
                logbuf,
                drain_gate.clone(),
                receipts.clone(),
                artifact_io.clone(),
            ),
            workspace: workspace::Handle::new(workspace_rx, handle_state, receipts.clone()),
            cache: cache::Handle::new(cache_rx, handle_cache, receipts.clone()),
            upgrader_rx,
            upgrade_deps,
            upgrade_uplink,
            drain_gate,
            exit_tx,
            exit_rx,
            receipts,
            artifact_io,
        }
    }

    /// 覆盖升级依赖（测试注入 fake 下载器/启动器/当前二进制路径）。须在 `run`
    /// 前调用；`Agent::new` 已覆盖为 real，测试据此注入 fake（不真下载/真重启）。
    pub fn with_upgrader_deps(mut self, deps: UpgradeDeps) -> Self {
        self.upgrade_deps = deps;
        self
    }

    /// 覆盖产物传输缝（票 #74；测试注入 fake 上传/下载）。须在 `run` 前调用；
    /// `Agent::new` 已装配 real（reqwest），测试据此注入 fake（不发真请求）。
    pub fn with_artifact_io(mut self, io: Arc<dyn artifacts::ArtifactIo>) -> Self {
        self.artifact_io = io;
        self
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

    /// 缓存共享状态（集成测试用它 restore/save/列表/删除真实缓存目录、读
    /// cache_bytes）。
    pub fn cache_state(&self) -> Cache {
        self.cache_state.clone()
    }

    /// 常驻运行：占位模块循环 + 通道重连循环。断线即指数退避重连、永久
    /// 重试不自杀（认证拒绝/版本拒连/网络失败一律重试，日志写明原因）。
    /// `shutdown` 收到 `true` 时退出（测试/将来服务化用；生产主进程常驻）。
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        // 各模块占位循环（收下行指令即记日志；真实执行随后续批次）。
        // 产物传输缝注入（票 #74）：`with_artifact_io`（run 前）可覆盖组合根
        // 默认装配的 real io——Handle 持可替换位，与 logbuf/workspace 的
        // set_live 同款时机语义（spawn 前注入，之后不再变）。
        self.runner.set_artifact_io(self.artifact_io.clone());
        let runner_task = tokio::spawn(self.runner.run());
        let workspace_task = tokio::spawn(self.workspace.run());
        let cache_task = tokio::spawn(self.cache.run());
        // upgrader Handle 延后到此装配（`with_upgrader_deps` 已在 `run` 前注入）：
        // 消费 upgrader_rx / upgrade_deps / exit_tx，克隆其余共享状态。token 取
        // channel_cfg（下载 Bearer），api_url 取 config（相对 download_url 解析），
        // agent.json 路径取 config（失败计数持久化）。
        let upgrader_handle = upgrader::Handle::new(
            self.upgrader_rx,
            self.upgrade_uplink.clone(),
            self.drain_gate.clone(),
            self.in_flight.clone(),
            self.channel_cfg.token.clone(),
            self.config.api_url.clone(),
            self.upgrade_deps,
            self.config.agent_json_path(),
            Some(self.exit_tx.clone()),
            self.receipts.clone(),
        );
        let upgrader_task = tokio::spawn(upgrader_handle.run());

        // 工作区占用采样循环（ADR-0011/0019：低频后台遍历，spawn 先采样一次；
        // 间隔取自 channel_cfg——测试注入短间隔避免真实 10 分钟 sleep）。
        let sampler_task = self
            .workspace_sampler
            .clone()
            .spawn(self.channel_cfg.workspace_sample_interval);

        // 容器探测周期循环（ADR-0018：周期 `docker version` →
        // `sisyphus/container=docker` 标签随 metadata 上报）。经
        // `channel_cfg.labels.probe_handle()` 取探测句柄——PlatformLabels 返回
        // Some（spawn 周期探测）；StaticLabels 等测试静态源返回 None（不探测，
        // 避免测试依赖宿主 docker）。首帧即探（首次连接前结果就绪）。
        let probe_task = self.channel_cfg.labels.probe_handle().map(|p| {
            p.spawn_refresh(
                channel::CONTAINER_PROBE_INTERVAL,
                container::DOCKER_BIN.to_string(),
            )
        });

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
                    &self.cache_state,
                    &self.upgrade_uplink,
                ) => r,
                _ = shutdown_requested(&mut shutdown) => break,
                // 升级成功（upgrader spawn 新进程后置位）→ 旧进程退出，跳出重连循环。
                _ = upgrade_exit_requested(&mut self.exit_rx) => break,
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
                _ = upgrade_exit_requested(&mut self.exit_rx) => break,
            }
        }

        runner_task.abort();
        workspace_task.abort();
        cache_task.abort();
        upgrader_task.abort();
        sampler_task.abort();
        if let Some(t) = probe_task {
            t.abort();
        }
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

/// 是否收到升级成功信号（upgrader spawn 新进程后置位 watch；ADR-0017：旧进程
/// 退出，新进程接管）。与 [`shutdown_requested`] 同形——电平触发，迟到订阅亦见。
async fn upgrade_exit_requested(rx: &mut watch::Receiver<bool>) -> bool {
    if *rx.borrow() {
        return true;
    }
    match rx.changed().await {
        Ok(_) => *rx.borrow(),
        Err(_) => false,
    }
}

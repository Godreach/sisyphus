//! 集成测试：真实 tonic ↔ 真实 agent 组合根（loopback，不起独立进程）。
//!
//! fake Server：消费 proto 契约、实现 `AgentChannel` service——握手/认证
//! （Bearer token）/版本回发/心跳收集/下行指令注入/断连控制。agent 侧经
//! [`sisyphus_agent::Agent::with_channel_config`] 注入短心跳间隔与短退避
//! （0 抖动）求确定性；磁盘采样器用真实 [`PlatformDiskSampler`]（临时数据
//! 目录）。断言面全是经通道帧的假 Server 观测（握手/心跳/在途上报/分派
//! 收帧），不停靠 agent 内部循环细节（B2c 同纪律）。
//!
//! 覆盖本票验收：握手认证（无/错 token 拒连 + 永久重试不自杀）、版本窗口
//! 过新拒连、心跳 + 系统标签 + 磁盘占用上报、指数退避重连（重连 = 新握手
//! + 认证 + 在途上报 + 标签刷新）、下行分派骨架（各模块占位 handle 可收）。
//!
//! 日志 seq 缓冲补传（票 B3-T3，ADR-0007/0013）：断线 → 缓冲续写 → 重连幂等
//! 重放——全量到达不丢不乱序。用例直接驱动
//! [`sisyphus_agent::channel::run_connection`]（补传 + 孤儿清理的持有缝）而非
//! 经 `Agent::run`：要注入可控 `in_flight` 集（判定孤儿——本批 runner 未实现，
//! 在途集恒空；真实运行中 job 由 runner #59 维护），并显式控制两次连接
//! （连接 A 活体 → 断线 → 连接 B 重放）。
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sisyphus_agent::Agent;
use sisyphus_agent::channel::{Backoff, ChannelConfig, PlatformDiskSampler, StaticLabels};
use sisyphus_agent::config::{self, Overrides};
use sisyphus_proto::agent::{
    CacheCommand, CacheDeleteRequest, CacheList, ChannelMessage, Handshake, JobReported,
    UpgradeCommand, Version, WorkspaceCommand, WorkspaceList,
    agent_channel_server::{AgentChannel, AgentChannelServer},
    cache_command::Kind as CacheKind,
    channel_message::Kind,
    workspace_command::Kind as WorkspaceKind,
};
use tokio::sync::{RwLock, mpsc, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

/// 系统标签 metadata 头名（与 agent 侧 channel.rs 常量同形——fake 按契约
/// 名消费，不引用被测常量，防「自己校验自己」）。
const META_OS: &str = "x-sisyphus-os";
const META_ARCH: &str = "x-sisyphus-arch";
const META_CONTAINER: &str = "x-sisyphus-container";

/// 与 workspace 同版本（兼容窗口内）。
fn version(major: u32, minor: u32, patch: u32) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// 等到谓词成立或超时（异步路径轮询驱动，避免 flaky；5s 上限）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..50 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("条件在 5s 内未成立");
}

// ============================================================
// fake Server
// ============================================================

/// fake Server 的可观测状态与行为旋钮（Arc 共享：测试与 service 同持）。
struct FakeState {
    /// 期望的 Agent token（None = 不校验）。
    expect_token: Option<String>,
    /// token 已停用（停用 = 认证拒绝，镜像 Server 侧「停用即拒连」语义）。
    token_disabled: AtomicBool,
    /// 回发的 Server 版本（测试可拨过新）。
    server_version: Version,
    /// 握手后立即断开（模拟 Server 掉线；可中途翻转测恢复）。
    drop_after_handshake: AtomicBool,
    /// 连接尝试计数（每次 connect 调用）。
    attempts: Mutex<usize>,
    /// 每次连接的 Agent 握手（名 + 版本）。
    handshakes: Mutex<Vec<(String, Version)>>,
    /// 每次连接的系统标签（metadata 头名 → 值）。
    labels: Mutex<Vec<Vec<(String, String)>>>,
    /// 每次连接是否带 authorization 头。
    token_present: Mutex<Vec<bool>>,
    /// 收到的心跳（含磁盘占用）。
    heartbeats: Mutex<Vec<sisyphus_proto::agent::Heartbeat>>,
    /// 收到的在途上报。
    reported: Mutex<Vec<JobReported>>,
    /// 收到的日志帧（LogBatch，seq 幂等重放断言面）。
    log_batches: Mutex<Vec<sisyphus_proto::agent::LogBatch>>,
    /// 收到的工作区列表响应（WorkspaceList，列表/清理集成断言面）。
    workspace_lists: Mutex<Vec<WorkspaceList>>,
    /// 收到的缓存列表响应（CacheList，列表/删除集成断言面，ADR-0012）。
    cache_lists: Mutex<Vec<CacheList>>,
    /// 活动会话的下行发送器（测试注入下行指令用）。
    sessions: Mutex<Vec<mpsc::Sender<Result<ChannelMessage, Status>>>>,
    /// 断开信号：会话读取任务 select 监听，触发即关流（模拟 Server 中途掉线）。
    /// watch = 电平触发（迟到订阅/忙中任务回到循环即见），比 Notify 边沿触发
    /// 抗竞态——不漏断连信号。
    drop_signal: watch::Sender<bool>,
}

impl FakeState {
    fn attempts(&self) -> usize {
        *self.attempts.lock().expect("锁")
    }
    fn handshakes(&self) -> Vec<(String, Version)> {
        self.handshakes.lock().expect("锁").clone()
    }
    fn labels(&self) -> Vec<Vec<(String, String)>> {
        self.labels.lock().expect("锁").clone()
    }
    fn token_present(&self) -> Vec<bool> {
        self.token_present.lock().expect("锁").clone()
    }
    fn heartbeats(&self) -> Vec<sisyphus_proto::agent::Heartbeat> {
        self.heartbeats.lock().expect("锁").clone()
    }
    fn reported(&self) -> Vec<JobReported> {
        self.reported.lock().expect("锁").clone()
    }
    fn log_batches(&self) -> Vec<sisyphus_proto::agent::LogBatch> {
        self.log_batches.lock().expect("锁").clone()
    }
    fn workspace_lists(&self) -> Vec<WorkspaceList> {
        self.workspace_lists.lock().expect("锁").clone()
    }
    fn cache_lists(&self) -> Vec<CacheList> {
        self.cache_lists.lock().expect("锁").clone()
    }
    fn last_session_tx(&self) -> mpsc::Sender<Result<ChannelMessage, Status>> {
        self.sessions
            .lock()
            .expect("锁")
            .last()
            .cloned()
            .expect("应有会话")
    }
    /// 断开全部活动会话（模拟 Server 中途掉线）：置位 drop watch（各会话读取
    /// 任务回到循环即见并退出、drop 发送器）+ 清注册表（drop 注册表的发送器
    /// 克隆）——全部发送器消失 → 对端流 EOF → agent 走重连路径。
    fn drop_all_sessions(&self) {
        let _ = self.drop_signal.send(true);
        self.sessions.lock().expect("锁").clear();
    }
}

/// fake Server：握手回发、Bearer 认证、心跳/在途收集、下行注入、断连控制。
struct FakeServer {
    state: Arc<FakeState>,
}

#[tonic::async_trait]
impl AgentChannel for FakeServer {
    type ConnectStream = ReceiverStream<Result<ChannelMessage, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<ChannelMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let state = self.state.clone();
        *state.attempts.lock().expect("锁") += 1;

        // 认证：expect_token 配置时校验 `Authorization: Bearer <token>`
        // （镜像 Server 侧 bearer_token 语义的契约面）。
        let auth = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim)
            .map(str::to_string);
        state.token_present.lock().expect("锁").push(auth.is_some());
        let expected_matches = auth.as_deref() == state.expect_token.as_deref();
        if !expected_matches || state.token_disabled.load(Ordering::SeqCst) {
            return Err(Status::unauthenticated(
                "fake: Agent token 无效、缺失或已停用",
            ));
        }

        // 系统标签随连接呈送（连接面事实）。
        let labels = [META_OS, META_ARCH, META_CONTAINER]
            .into_iter()
            .filter_map(|header| {
                request
                    .metadata()
                    .get(header)
                    .and_then(|v| v.to_str().ok())
                    .map(|value| (header.to_string(), value.to_string()))
            })
            .collect();
        state.labels.lock().expect("锁").push(labels);

        // 首帧必须是握手（镜像 Server 侧语义）。
        let mut inbound = request.into_inner();
        let mut agent_version = None;
        while let Some(msg) = inbound
            .message()
            .await
            .map_err(|e| Status::internal(e.to_string()))?
        {
            if let Some(Kind::Handshake(h)) = msg.kind {
                agent_version = h.agent_version;
                state
                    .handshakes
                    .lock()
                    .expect("锁")
                    .push((h.agent_name, h.agent_version.unwrap_or_default()));
                break;
            }
        }
        if agent_version.is_none() {
            return Err(Status::invalid_argument("fake: 首帧必须是握手"));
        }

        let (tx, rx) = mpsc::channel(64);
        // 会话注册仅限非 drop 模式：drop 模拟「连接即刻断开」——会话发送器
        // 一旦被外部持有，mpsc 通道不会关闭，对端就读不到流结束（仿真失真）。
        if !state.drop_after_handshake.load(Ordering::SeqCst) {
            state.sessions.lock().expect("锁").push(tx.clone());
        }

        // 握手后立即断开（模拟 Server 掉线；可中途翻转测恢复）：回发握手后
        // 不再持有会话发送器 → rx 关闭 → 对端流 EOF。
        if state.drop_after_handshake.load(Ordering::SeqCst) {
            let _ = tx
                .send(Ok(ChannelMessage {
                    kind: Some(Kind::Handshake(Handshake {
                        agent_version: Some(state.server_version),
                        agent_name: "fake-server".into(),
                    })),
                }))
                .await;
            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        tokio::spawn(async move {
            if tx
                .send(Ok(ChannelMessage {
                    kind: Some(Kind::Handshake(Handshake {
                        agent_version: Some(state.server_version),
                        agent_name: "fake-server".into(),
                    })),
                }))
                .await
                .is_err()
            {
                return;
            }
            // 订阅断开信号（电平触发：迟订阅/忙中任务回到循环即见已置位）。
            let mut drop_rx = state.drop_signal.subscribe();
            loop {
                tokio::select! {
                    _ = drop_rx.changed() => break, // Server 掉线：关流
                    msg = inbound.message() => {
                        let Ok(Some(msg)) = msg else { break };
                        match msg.kind {
                            Some(Kind::Heartbeat(hb)) => state.heartbeats.lock().expect("锁").push(hb),
                            Some(Kind::JobReported(r)) => state.reported.lock().expect("锁").push(r),
                            Some(Kind::LogBatch(b)) => state.log_batches.lock().expect("锁").push(b),
                            Some(Kind::WorkspaceList(l)) => {
                                state.workspace_lists.lock().expect("锁").push(l)
                            }
                            Some(Kind::CacheList(l)) => {
                                state.cache_lists.lock().expect("锁").push(l)
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// 起 fake Server（真实 tonic，loopback socket），返回地址与 JoinHandle。
async fn spawn_fake(state: Arc<FakeState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = FakeServer { state };
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentChannelServer::new(server))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });
    (addr, handle)
}

fn fake_state(expect_token: Option<&str>, server_version: Version) -> Arc<FakeState> {
    Arc::new(FakeState {
        expect_token: expect_token.map(str::to_string),
        token_disabled: AtomicBool::new(false),
        server_version,
        drop_after_handshake: AtomicBool::new(false),
        attempts: Mutex::new(0),
        handshakes: Mutex::new(Vec::new()),
        labels: Mutex::new(Vec::new()),
        token_present: Mutex::new(Vec::new()),
        heartbeats: Mutex::new(Vec::new()),
        reported: Mutex::new(Vec::new()),
        log_batches: Mutex::new(Vec::new()),
        workspace_lists: Mutex::new(Vec::new()),
        cache_lists: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
        drop_signal: watch::channel(false).0,
    })
}

// ============================================================
// agent 装配
// ============================================================

/// 注入短心跳/短退避（0 抖动）的通道配置——用例确定性，不依赖真实
/// 15s 心跳与 1s 退避。工作区采样间隔默认取真实 10 分钟（采样用例另行注入
/// 短间隔）。
fn channel_config(server_url: String, token: Option<&str>, data_dir: &Path) -> ChannelConfig {
    ChannelConfig {
        server_url,
        token: token.map(str::to_string),
        heartbeat_interval: Duration::from_millis(200),
        backoff: Backoff::with_params(Duration::from_millis(50), Duration::from_millis(300), 0.0),
        labels: Arc::new(StaticLabels),
        disk: Arc::new(PlatformDiskSampler::new(data_dir.to_path_buf())),
        workspace_sample_interval: sisyphus_agent::workspace::DEFAULT_WORKSPACE_SAMPLE_INTERVAL,
    }
}

/// 组装组合根并 spawn `Agent::run`。返回
/// （关闭发送端, 收帧观测, 日志缓冲, JoinHandle）——日志缓冲供测试直接喂
/// 事件（断线续写断言面）。
fn spawn_agent(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
) -> (
    watch::Sender<bool>,
    sisyphus_agent::ReceiptLog,
    sisyphus_agent::logbuf::LogBuffer,
    tokio::task::JoinHandle<()>,
) {
    let cfg = config::Config::load(
        &Overrides {
            server_url: Some(server_url.clone()),
            data_dir: Some(data_dir.to_path_buf()),
            ..Overrides::default()
        },
        &Overrides::default(),
    )
    .expect("配置");
    let (ws_state, ws_sampler) = build_workspace(&cfg);
    let cache_state = build_cache(&cfg);
    let agent = Agent::with_channel_config(
        cfg,
        channel_config(server_url, token, data_dir),
        ws_state,
        ws_sampler,
        cache_state,
    );
    let receipts = agent.receipts();
    let logbuf = agent.logbuf();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, receipts, logbuf, task)
}

/// 组装组合根并 spawn `Agent::run`，额外返回工作区共享状态（集成测试用它
/// 在真实文件系统上 resolve/清理工作区目录）。`sample_interval` 注入工作区
/// 采样间隔（采样用例传短间隔避免真实 sleep）。
fn spawn_agent_ws(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
    sample_interval: Duration,
) -> (
    watch::Sender<bool>,
    sisyphus_agent::workspace::Workspace,
    sisyphus_agent::ReceiptLog,
    tokio::task::JoinHandle<()>,
) {
    let cfg = config::Config::load(
        &Overrides {
            server_url: Some(server_url.clone()),
            data_dir: Some(data_dir.to_path_buf()),
            ..Overrides::default()
        },
        &Overrides::default(),
    )
    .expect("配置");
    let (ws_state, ws_sampler) = build_workspace(&cfg);
    let cache_state = build_cache(&cfg);
    let mut ch_cfg = channel_config(server_url, token, data_dir);
    ch_cfg.workspace_sample_interval = sample_interval;
    let agent = Agent::with_channel_config(cfg, ch_cfg, ws_state, ws_sampler, cache_state);
    let receipts = agent.receipts();
    let ws = agent.workspace_state();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, ws, receipts, task)
}

/// 组装组合根并 spawn `Agent::run`，额外返回缓存共享状态（集成测试用它
/// 在真实文件系统上 save/restore/列表/删除缓存目录）。与 `spawn_agent_ws`
/// 同款 + cache 状态。
fn spawn_agent_cache(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
    sample_interval: Duration,
) -> (
    watch::Sender<bool>,
    sisyphus_agent::workspace::Workspace,
    sisyphus_agent::cache::Cache,
    sisyphus_agent::ReceiptLog,
    tokio::task::JoinHandle<()>,
) {
    let cfg = config::Config::load(
        &Overrides {
            server_url: Some(server_url.clone()),
            data_dir: Some(data_dir.to_path_buf()),
            ..Overrides::default()
        },
        &Overrides::default(),
    )
    .expect("配置");
    let (ws_state, ws_sampler) = build_workspace(&cfg);
    let cache_state = build_cache(&cfg);
    let mut ch_cfg = channel_config(server_url, token, data_dir);
    ch_cfg.workspace_sample_interval = sample_interval;
    let agent = Agent::with_channel_config(cfg, ch_cfg, ws_state, ws_sampler, cache_state);
    let receipts = agent.receipts();
    let ws = agent.workspace_state();
    let cache = agent.cache_state();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, ws, cache, receipts, task)
}

/// 装配工作区共享状态 + 低频采样器（挂 usage），与 `Agent::new` 同款。
fn build_workspace(
    cfg: &config::Config,
) -> (
    sisyphus_agent::workspace::Workspace,
    std::sync::Arc<sisyphus_agent::workspace::WorkspaceSampler>,
) {
    let root = cfg.workspaces_dir();
    let sampler = std::sync::Arc::new(sisyphus_agent::workspace::WorkspaceSampler::new(
        root.clone(),
    ));
    let state = sisyphus_agent::workspace::Workspace::new(root).with_usage(sampler.clone());
    (state, sampler)
}

/// 装配缓存共享状态（ADR-0012），与 `Agent::new` 同款：缓存根 + 容量上限
/// （默认 20 GiB；cache 集成测试用 `Cache::new` 直构小容量覆盖）。
fn build_cache(cfg: &config::Config) -> sisyphus_agent::cache::Cache {
    sisyphus_agent::cache::Cache::new(cfg.cache_dir(), cfg.cache_capacity_bytes())
}

/// 直接驱动单次连接（不经 Agent::run 重连循环）——认证/版本拒绝的断言面。
async fn connect_once(
    server_url: String,
    token: Option<&str>,
    data_dir: &Path,
) -> Result<(), sisyphus_agent::channel::ChannelError> {
    let cfg = channel_config(server_url, token, data_dir);
    let dispatch = dummy_dispatch();
    let logbuf =
        sisyphus_agent::logbuf::LogBuffer::new(data_dir.join("logbuf"), Duration::from_secs(60));
    let workspace = sisyphus_agent::workspace::Workspace::new(data_dir.join("workspaces"));
    let cache = sisyphus_agent::cache::Cache::new(data_dir.join("cache"), 0);
    let runner_uplink = sisyphus_agent::runner::RunnerUplink::new();
    sisyphus_agent::channel::run_connection(
        &cfg,
        &dispatch,
        Arc::new(RwLock::new(Vec::new())),
        &logbuf,
        &runner_uplink,
        &workspace,
        &cache,
    )
    .await
}

fn dummy_dispatch() -> sisyphus_agent::channel::Dispatch {
    let (runner_tx, _) = mpsc::channel(4);
    let (upgrader_tx, _) = mpsc::channel(4);
    let (workspace_tx, _) = mpsc::channel(4);
    let (cache_tx, _) = mpsc::channel(4);
    sisyphus_agent::channel::Dispatch {
        runner: runner_tx,
        upgrader: upgrader_tx,
        workspace: workspace_tx,
        cache: cache_tx,
    }
}

// ============================================================
// 用例
// ============================================================

/// 握手认证：有效 token 连上，心跳（含磁盘占用）与在途上报落 fake，系统
/// 标签随连接呈送（os/arch 有值、容器占位不置）。
#[tokio::test]
async fn valid_token_establishes_session_with_heartbeat_labels_and_inflight() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, _receipts, _logbuf, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 握手 + 认证通过 + 标签随连接呈送。
    wait_until(|| async { !state.handshakes().is_empty() }).await;
    let (name, v) = &state.handshakes()[0];
    assert!(!name.is_empty(), "握手携带主机名");
    assert_eq!(v, &version(1, 0, 0), "握手携带 Agent 版本");
    assert!(state.token_present()[0], "token 随连接呈送");
    let labels = &state.labels()[0];
    assert!(
        labels.iter().any(|(k, _)| k == META_OS),
        "os 标签随连接上报"
    );
    assert!(
        labels.iter().any(|(k, _)| k == META_ARCH),
        "arch 标签随连接上报"
    );
    assert!(
        !labels.iter().any(|(k, _)| k == META_CONTAINER),
        "测试注入 StaticLabels（os/arch，无容器探测）——确定性，不依赖宿主 docker"
    );

    // 在途上报：连接建立即上报（本批为空集，机制在）。
    wait_until(|| async { !state.reported().is_empty() }).await;
    assert!(state.reported()[0].job_ids.is_empty(), "本批在途为空集");

    // 心跳：15s 语义经注入短间隔验证，附带真实磁盘占用（卷级 + 占位 0）。
    wait_until(|| async { !state.heartbeats().is_empty() }).await;
    let heartbeats = state.heartbeats();
    let disk = heartbeats
        .last()
        .expect("心跳")
        .disk
        .as_ref()
        .expect("磁盘占用");
    assert_eq!(disk.cache_bytes, 0, "缓存记账占位（cache 批次）");
    assert_eq!(disk.workspace_bytes, 0, "工作区采样占位（workspace 批次）");
    assert!(
        disk.volumes.first().is_some_and(|v| v.total_bytes > 0),
        "卷级磁盘占用应真实采样"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 任务应正常退出");
    server_task.abort();
}

/// 认证拒绝：缺 token / 错 token 单次连接即被拒（不吞错误）；Agent::run
/// 层面对拒绝永久退避重试不自杀。
#[tokio::test]
async fn rejects_missing_and_wrong_token_and_retries_forever() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    // 缺 token：agent 不带凭据也连（被拒后走退避——不自杀），fake 侧可见
    // 无 authorization 头。
    let err = connect_once(format!("http://{addr}"), None, dir.path()).await;
    assert!(err.is_err(), "缺 token 应拒连");
    assert!(
        !state.token_present().first().copied().unwrap_or(true),
        "缺 token 时不带凭据头"
    );

    // 错 token：单次连接被拒。
    let err = connect_once(format!("http://{addr}"), Some("sisa_wrong"), dir.path()).await;
    assert!(err.is_err(), "错 token 应拒连");

    // Agent::run 层：错 token 下永久退避重连（attempts 持续增长），进程
    // 不自杀（run 任务仍在跑）；shutdown 干净退出。
    let before = state.attempts();
    let (shutdown_tx, _receipts, _logbuf, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_wrong"));
    wait_until(|| async { state.attempts() >= before + 2 }).await;
    assert!(!agent_task.is_finished(), "拒连不自杀：run 循环仍在重试");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 任务应正常退出");
    server_task.abort();
}

/// 停用拒连：token 正确但已停用 → 连接被拒（镜像 Server 侧「停用即拒连」，
/// ADR-0007）；Agent 侧与错 token 同路径——退避重试不自杀。
#[tokio::test]
async fn rejects_disabled_token_and_retries() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    state.token_disabled.store(true, Ordering::SeqCst);
    let (addr, server_task) = spawn_fake(state.clone()).await;

    // 单次连接被拒（停用与无效/缺失同一 unauthenticated 语义）。
    let err = connect_once(format!("http://{addr}"), Some("sisa_abc"), dir.path()).await;
    assert!(err.is_err(), "停用 token 应拒连");

    // Agent::run 层：停用下永久退避重试（attempts 持续增长），进程不自杀。
    let before = state.attempts();
    let (shutdown_tx, _receipts, _logbuf, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));
    wait_until(|| async { state.attempts() >= before + 2 }).await;
    assert!(
        !agent_task.is_finished(),
        "停用拒连不自杀：run 循环仍在重试"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 任务应正常退出");
    server_task.abort();
}

/// 版本窗口：Server 过新（2.0.0 > 1.0.0）→ Agent 拒连并明确报错（ADR-0010/
/// 0017）。握手已互见（fake 记录到 Agent 握手）后，Agent 侧裁决拒绝。
#[tokio::test]
async fn rejects_server_too_new_version_with_clear_error() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(2, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let err = connect_once(format!("http://{addr}"), Some("sisa_abc"), dir.path()).await;
    let err = err.expect_err("Server 过新应拒连");
    assert!(
        err.to_string().contains("过新"),
        "报错应明确版本窗口语义：{}",
        err
    );

    // 握手确实发生（fake 收到了 Agent 的版本握手）——拒连发生在版本裁决。
    assert_eq!(state.handshakes().len(), 1, "过新场景握手已互见");
    server_task.abort();
}

/// 指数退避重连：握手后断线 → 自动重连；每次重连 = 新握手 + 认证 +
/// 在途上报 + 标签刷新；断连恢复后回到稳定会话（心跳续报）。
#[tokio::test]
async fn reconnects_after_drop_with_full_rehandshake() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    state.drop_after_handshake.store(true, Ordering::SeqCst);
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, _receipts, _logbuf, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 断线期间多次重连：每次都是完整握手 + 认证 + 标签刷新。
    wait_until(|| async { state.handshakes().len() >= 2 }).await;
    let n = state.handshakes().len();
    assert!(n >= 2, "断线后应自动重连并重新握手：{n} 次");
    assert_eq!(state.labels().len(), n, "每次重连都重新刷新标签");
    assert!(
        state
            .labels()
            .iter()
            .all(|labels| labels.iter().any(|(k, _)| k == META_OS)),
        "每次连接的 os 标签都在"
    );
    assert!(
        state.token_present().iter().all(|p| *p),
        "每次连接都带 token 认证"
    );
    // 在途上报在「连接存活」期间送达（drop 模式连接随断线丢弃，其上报
    // 是否到达是竞态）；恢复后的稳定会话必须重新上报。
    let reported_before = state.reported().len();

    // 恢复：drop 关掉后下一轮重连进入稳定会话，心跳续报 + 在途重新上报。
    state.drop_after_handshake.store(false, Ordering::SeqCst);
    let heartbeats_before = state.heartbeats().len();
    wait_until(|| async { state.heartbeats().len() > heartbeats_before }).await;
    wait_until(|| async { state.reported().len() > reported_before }).await;

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 任务应正常退出");
    server_task.abort();
}

/// 下行分派骨架：JobSpec/Workspace/Cache/Upgrade 指令按类型分派到各模块
/// 占位 handle（收帧观测落账）；对端关流后 run 退出。
#[tokio::test]
async fn dispatches_downlink_frames_to_module_placeholders() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, receipts, _logbuf, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 等连接建立（fake 有会话发送器）。
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    let tx = state.last_session_tx();

    // 各类型指令各一帧：JobSpec → runner、Cancel → runner、Workspace →
    // workspace、Cache → cache、Upgrade → upgrader。
    for msg in [
        ChannelMessage {
            kind: Some(Kind::JobSpec(Box::new(sisyphus_proto::agent::JobSpec {
                job_id: "1".into(),
                ..Default::default()
            }))),
        },
        ChannelMessage {
            kind: Some(Kind::Cancel(sisyphus_proto::agent::CancelBuild {
                build_id: "1".into(),
                job_id: "1".into(),
            })),
        },
        ChannelMessage {
            kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
                kind: Some(WorkspaceKind::List(Default::default())),
            })),
        },
        ChannelMessage {
            kind: Some(Kind::CacheCmd(CacheCommand {
                kind: Some(CacheKind::List(Default::default())),
            })),
        },
        ChannelMessage {
            kind: Some(Kind::Upgrade(UpgradeCommand {
                package_name: "sisyphus-agent-1.0.0-linux-amd64.tar.gz".into(),
                sha256: "abc".into(),
                download_url: "http://example".into(),
            })),
        },
    ] {
        tx.send(Ok(msg)).await.expect("下发");
    }

    // 各模块占位 handle 均收到（收帧观测落账）。
    wait_until(|| async {
        let got = receipts.lock().expect("观测锁").clone();
        ["job_spec", "cancel", "workspace", "cache", "upgrade"]
            .iter()
            .all(|kind| got.contains(&kind.to_string()))
    })
    .await;

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 任务应正常退出");
    server_task.abort();
}

/// 日志 seq 缓冲补传（ADR-0007/0013 集成验收）：
/// 断线 → 缓冲续写 → 重连幂等重放——全量到达、seq 连续无重复、不丢不乱序。
///
/// 直接驱动 [`sisyphus_agent::channel::run_connection`]（补传 + 孤儿清理的
/// 持有缝），两次显式连接模拟「连接 A（活体转发）→ 断线 → 缓冲续写 →
/// 连接 B（重放补传）」。`in_flight` 可控注入：job-1 是在途任务（缓冲保留、
/// 重放补传）；job-2 是孤儿（补传后删除）。
#[tokio::test]
async fn buffers_logs_while_disconnected_and_backfills_on_reconnect() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let logbuf_dir = dir.path().join("logbuf");
    std::fs::create_dir_all(&logbuf_dir).expect("logbuf 目录");
    let logbuf = std::sync::Arc::new(sisyphus_agent::logbuf::LogBuffer::new(
        logbuf_dir,
        Duration::from_secs(60),
    ));
    let log_event = |data: &[u8]| sisyphus_proto::agent::LogEvent {
        seq: 0, // 缓冲层重编号
        kind: Some(sisyphus_proto::agent::log_event::Kind::Output(
            sisyphus_proto::agent::OutputChunk {
                stream: sisyphus_proto::agent::Stream::Stdout as i32,
                data: data.to_vec(),
            },
        )),
    };

    // 连接 A：在途 = job-1（运行中）。连接建立后活体转发 alpha/beta。
    let cfg_a = std::sync::Arc::new(channel_config(
        format!("http://{addr}"),
        Some("sisa_abc"),
        dir.path(),
    ));
    let dispatch_a = std::sync::Arc::new(dummy_dispatch());
    let in_flight_a = Arc::new(RwLock::new(vec!["job-1".to_string()]));
    let workspace_a = std::sync::Arc::new(sisyphus_agent::workspace::Workspace::new(
        dir.path().join("workspaces"),
    ));
    let cache_a = std::sync::Arc::new(sisyphus_agent::cache::Cache::new(dir.path().join("cache"), 0));
    let (cfg_ha, dispatch_ha, logbuf_ha) = (cfg_a.clone(), dispatch_a.clone(), logbuf.clone());
    let runner_uplink_a = sisyphus_agent::runner::RunnerUplink::new();
    let conn_a = tokio::spawn(async move {
        sisyphus_agent::channel::run_connection(
            &cfg_ha,
            &dispatch_ha,
            in_flight_a,
            &logbuf_ha,
            &runner_uplink_a,
            &workspace_a,
            &cache_a,
        )
        .await
    });
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    logbuf
        .append("job-1", 0, log_event(b"alpha"))
        .await
        .expect("落盘");
    logbuf
        .append("job-1", 0, log_event(b"beta"))
        .await
        .expect("落盘");
    wait_until(|| async { state.log_batches().len() >= 2 }).await;

    // 断线：fake 断开全部会话 → 连接 A 结束；断线期间缓冲继续累计。
    state.drop_all_sessions();
    conn_a.await.expect("连接 A 结束").expect("连接 A 干净退出");
    logbuf
        .append("job-1", 0, log_event(b"gamma"))
        .await
        .expect("断线续写");
    logbuf
        .append("job-1", 0, log_event(b"delta"))
        .await
        .expect("断线续写");
    logbuf
        .append("job-2", 0, log_event(b"orphan"))
        .await
        .expect("孤儿落盘");

    // 连接 B（重连）：在途仍 = job-1。重放 job-1 全段（0..3）+ 孤儿 job-2
    // 补传后删除。连接期内新事件（epsilon）经活体转发。
    let cfg_b = std::sync::Arc::new(channel_config(
        format!("http://{addr}"),
        Some("sisa_abc"),
        dir.path(),
    ));
    let dispatch_b = std::sync::Arc::new(dummy_dispatch());
    let in_flight_b = Arc::new(RwLock::new(vec!["job-1".to_string()]));
    let workspace_b = std::sync::Arc::new(sisyphus_agent::workspace::Workspace::new(
        dir.path().join("workspaces"),
    ));
    let cache_b = std::sync::Arc::new(sisyphus_agent::cache::Cache::new(dir.path().join("cache"), 0));
    let (cfg_hb, dispatch_hb, logbuf_hb) = (cfg_b.clone(), dispatch_b.clone(), logbuf.clone());
    let runner_uplink_b = sisyphus_agent::runner::RunnerUplink::new();
    let conn_b = tokio::spawn(async move {
        sisyphus_agent::channel::run_connection(
            &cfg_hb,
            &dispatch_hb,
            in_flight_b,
            &logbuf_hb,
            &runner_uplink_b,
            &workspace_b,
            &cache_b,
        )
        .await
    });
    // 等重连建立 + job-2 孤儿缓冲删除（补传后删）。
    wait_until(|| async { state.handshakes().len() >= 2 }).await;
    wait_until(|| async { !logbuf.path("job-2", 0).exists() }).await;

    // job-1 全量到达 0..3、无缺无杂（补传是幂等重放：连接 B 从文件头重放
    // 整段——已活体送达的前缀 0,1 会重复上送，Server 按 seq 幂等吸收；故
    // 不要求「每 seq 恰好一次」也不要求整体非降，只要求：不丢、不缺、无
    // 越界杂 seq）。
    wait_until(|| async {
        let seen: Vec<u64> = state
            .log_batches()
            .iter()
            .filter(|b| b.job_id == "job-1")
            .map(|b| b.start_seq)
            .collect();
        (0..4).all(|i| seen.contains(&i))
    })
    .await;
    let seen: Vec<u64> = state
        .log_batches()
        .iter()
        .filter(|b| b.job_id == "job-1")
        .map(|b| b.start_seq)
        .collect();
    let mut unique: Vec<u64> = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique,
        vec![0, 1, 2, 3],
        "job-1 日志应全量到达 0..3（不丢、无缺、无杂）：{seen:?}"
    );
    // job-2 的孤儿事件重放补传后删除——fake 收到了它的日志（取证保留到
    // 补传完成才删），缓冲文件已不在。
    assert!(
        state
            .log_batches()
            .iter()
            .any(|b| b.job_id == "job-2" && b.start_seq == 0),
        "孤儿 job-2 的日志应补传（取证）"
    );
    assert!(!logbuf.path("job-2", 0).exists(), "孤儿 job-2 缓冲删除");
    assert!(
        logbuf.path("job-1", 0).exists(),
        "在途 job-1 的缓冲保留（不删）"
    );

    // 收尾：让连接 B 自然结束（fake 关流）后清理。
    state.drop_all_sessions();
    conn_b.await.expect("连接 B 结束").expect("连接 B 干净退出");
    server_task.abort();
}

/// 工作区列表指令（ADR-0011 集成验收）：fake Server 下发 `WorkspaceListRequest`
/// → Agent 遍历真实工作区根、还原 (pipeline, job, path, last_used) 上行。在
/// Agent 的工作区根上真实 resolve 出两个工作区（含一个清洗冲突 → -2 后缀），
/// 断言 fake 收到的列表还原真名。
#[tokio::test]
async fn workspace_list_command_reports_real_filesystem_entries() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, ws, _receipts, agent_task) = spawn_agent_ws(
        dir.path(),
        format!("http://{addr}"),
        Some("sisa_abc"),
        Duration::from_secs(60),
    );

    // 在 Agent 的工作区根上真实 resolve 两个工作区（含清洗冲突）。
    let a = ws.resolve("pipe-a", "job 1").expect("resolve");
    let b = ws.resolve("pipe-a", "job_1").expect("resolve 冲突");
    std::fs::write(a.join("out"), b"x").expect("写入产出");
    std::fs::write(b.join("out"), b"y").expect("写入产出");
    // b 清洗冲突 → job_1-2；a 与 b 真名不同。
    assert!(a.file_name() != b.file_name(), "冲突追加后缀");

    // 等连接建立后下发列表指令。
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    let tx = state.last_session_tx();
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
            kind: Some(WorkspaceKind::List(Default::default())),
        })),
    }))
    .await
    .expect("下发列表指令");

    // fake 收到 WorkspaceList，含两个条目、真名还原。
    wait_until(|| async { !state.workspace_lists().is_empty() }).await;
    let list = &state.workspace_lists()[0];
    assert_eq!(list.entries.len(), 2, "两个工作区都列出");
    let mut named: Vec<(String, String)> = list
        .entries
        .iter()
        .map(|e| (e.pipeline.clone(), e.job.clone()))
        .collect();
    named.sort();
    assert_eq!(
        named,
        vec![
            ("pipe-a".to_string(), "job 1".into()),
            ("pipe-a".to_string(), "job_1".into())
        ],
        "列表还原真名（含冲突后缀目录还原原始 job 名）"
    );
    for e in &list.entries {
        assert!(Path::new(&e.path).is_dir(), "path 指向真实目录");
        assert!(e.last_used_at_ms > 0, "last_used 取自标记");
    }

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 工作区清理指令（ADR-0011 集成验收）：fake Server 下发 `WorkspaceCleanRequest`
///（单 job / 单 pipeline / 全清三态分别覆盖）→ Agent 删树作用于真实文件系统，
/// 永不触碰缓存根。
#[tokio::test]
async fn workspace_clean_command_removes_real_dirs_without_touching_cache() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, ws, _receipts, agent_task) = spawn_agent_ws(
        dir.path(),
        format!("http://{addr}"),
        Some("sisa_abc"),
        Duration::from_secs(60),
    );

    // 三个工作区：pipe-a/{job-1, job-2}、pipe-b/job-1。
    let a1 = ws.resolve("pipe-a", "job-1").expect("resolve");
    let a2 = ws.resolve("pipe-a", "job-2").expect("resolve");
    let b1 = ws.resolve("pipe-b", "job-1").expect("resolve");
    std::fs::write(a1.join("out"), b"x").expect("写");
    std::fs::write(a2.join("out"), b"x").expect("写");
    std::fs::write(b1.join("out"), b"x").expect("写");
    // 缓存根（工作区根的兄弟）放一个产物，清理永不触碰。
    let cache_file = dir
        .path()
        .join("cache")
        .join("pipe-a")
        .join("key")
        .join("artifact");
    std::fs::create_dir_all(cache_file.parent().unwrap()).expect("建缓存目录");
    std::fs::write(&cache_file, b"cached").expect("写缓存");

    let tx = {
        wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
        state.last_session_tx()
    };

    // 单 job：清 pipe-a/job-1。
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
            kind: Some(WorkspaceKind::Clean(
                sisyphus_proto::agent::WorkspaceCleanRequest {
                    pipeline: "pipe-a".into(),
                    job: "job-1".into(),
                },
            )),
        })),
    }))
    .await
    .expect("下发单 job 清理");
    wait_until(|| async { !a1.exists() }).await;
    assert!(a2.exists(), "同 pipeline 其它 job 保留");
    assert!(b1.exists(), "其它 pipeline 保留");
    assert!(cache_file.exists(), "缓存未被触碰");

    // 单 pipeline：清 pipe-b。
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
            kind: Some(WorkspaceKind::Clean(
                sisyphus_proto::agent::WorkspaceCleanRequest {
                    pipeline: "pipe-b".into(),
                    job: String::new(),
                },
            )),
        })),
    }))
    .await
    .expect("下发单 pipeline 清理");
    wait_until(|| async { !b1.exists() }).await;
    assert!(a2.exists(), "pipe-a 仍保留");
    assert!(cache_file.exists(), "缓存仍未被触碰");

    // 全清：清剩余 pipe-a。
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
            kind: Some(WorkspaceKind::Clean(
                sisyphus_proto::agent::WorkspaceCleanRequest {
                    pipeline: String::new(),
                    job: String::new(),
                },
            )),
        })),
    }))
    .await
    .expect("下发全清");
    wait_until(|| async { !a2.exists() }).await;
    assert!(ws.root().is_dir(), "工作区根本身保留");
    assert!(cache_file.exists(), "全清后缓存仍完好");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 工作区占用采样（ADR-0011/0019 集成验收）：注入短采样间隔，Agent 后台
/// 采样真实工作区占用 → 心跳 `DiskUsage.workspace_bytes` 可见该值。
#[tokio::test]
async fn workspace_usage_sample_visible_in_heartbeat() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    // 注入短采样间隔（50ms）避免真实 10 分钟 sleep。
    let (shutdown_tx, ws, _receipts, agent_task) = spawn_agent_ws(
        dir.path(),
        format!("http://{addr}"),
        Some("sisa_abc"),
        Duration::from_millis(50),
    );

    // 真实 resolve 一个工作区并写入已知大小产出。
    let job_dir = ws.resolve("pipe", "job").expect("resolve");
    let payload = vec![0u8; 4096];
    std::fs::write(job_dir.join("out.bin"), &payload).expect("写产出");

    // 心跳携带的 workspace_bytes 应反映采样值（4096；标记文件不计入）。
    wait_until(|| async {
        state
            .heartbeats()
            .iter()
            .any(|hb| hb.disk.as_ref().is_some_and(|d| d.workspace_bytes == 4096))
    })
    .await;
    // 至少有一帧心跳 workspace_bytes > 0，卷级仍真实（cache_bytes 仍占位 0）。
    let heartbeats = state.heartbeats();
    let hb = heartbeats
        .iter()
        .find(|hb| hb.disk.as_ref().is_some_and(|d| d.workspace_bytes > 0))
        .expect("应有心跳携带工作区占用");
    let disk = hb.disk.as_ref().expect("磁盘占用");
    assert_eq!(disk.workspace_bytes, 4096, "采样值在心跳中可见");
    assert_eq!(disk.cache_bytes, 0, "缓存记账仍占位（cache 批次）");
    assert!(
        disk.volumes.first().is_some_and(|v| v.total_bytes > 0),
        "卷级占用仍真实采样"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 缓存列表指令（ADR-0012 集成验收）：fake Server 下发 `CacheListRequest` →
/// Agent 遍历 registry + 落盘核对、上行 `CacheList`（key/大小/最近使用）。在
/// Agent 的缓存根上真实 save 两条缓存，断言 fake 收到的列表含真名 + 大小。
#[tokio::test]
async fn cache_list_command_reports_real_entries() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, _ws, cache, _receipts, agent_task) = spawn_agent_cache(
        dir.path(),
        format!("http://{addr}"),
        Some("sisa_abc"),
        Duration::from_secs(60),
    );

    // 在 Agent 的缓存根上真实 save 两条缓存（经 Cache::save，落 registry）。
    let seed_ws = tempfile::tempdir().expect("seed 工作区");
    std::fs::write(seed_ws.path().join("out"), b"hello").expect("写");
    cache
        .save(
            "pipe-a",
            &sisyphus_proto::agent::CacheSpec {
                key: "k1".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            seed_ws.path(),
        )
        .await;
    std::fs::write(seed_ws.path().join("out"), b"world!").expect("写");
    cache
        .save(
            "pipe-b",
            &sisyphus_proto::agent::CacheSpec {
                key: "k2".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            seed_ws.path(),
        )
        .await;
    // restore pipe-a/k1 一次（ADR-0012：restore 刷新 LRU 时钟；save-only 的 k2
    // 仍 last_used=0）。列表据此区分「被复用过」与「仅存未用」。
    let restore_ws = tempfile::tempdir().expect("restore 工作区");
    let _ = cache
        .restore(
            "pipe-a",
            &sisyphus_proto::agent::CacheSpec {
                key: "k1".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            restore_ws.path(),
        )
        .await;

    // 等连接建立后下发列表指令。
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    let tx = state.last_session_tx();
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::CacheCmd(CacheCommand {
            kind: Some(CacheKind::List(Default::default())),
        })),
    }))
    .await
    .expect("下发列表指令");

    // fake 收到 CacheList，含两条、真名 + 大小。
    wait_until(|| async { !state.cache_lists().is_empty() }).await;
    let list = &state.cache_lists()[0];
    assert_eq!(list.entries.len(), 2, "两条缓存都列出");
    let mut entries = list.entries.clone();
    entries.sort_by_key(|a| (a.pipeline.clone(), a.key.clone()));
    assert_eq!((entries[0].pipeline.clone(), entries[0].key.clone()), ("pipe-a".into(), "k1".into()));
    assert_eq!(entries[0].size_bytes, 5, "大小取自 registry");
    assert_eq!((entries[1].pipeline.clone(), entries[1].key.clone()), ("pipe-b".into(), "k2".into()));
    assert_eq!(entries[1].size_bytes, 6);
    // ADR-0012 时钟：被 restore 过的 k1 时钟 > 0；仅 save 的 k2 从未 restore = 0。
    assert!(
        entries[0].last_used_at_ms > 0,
        "k1 被 restore 刷新时钟 > 0"
    );
    assert_eq!(
        entries[1].last_used_at_ms, 0,
        "k2 仅 save 未 restore = 0（save 不刷新时钟）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 缓存删除指令（ADR-0012 集成验收）：单 key 删除 + 全清两态，作用于真实
/// 文件系统，经 fake Server 往返。
#[tokio::test]
async fn cache_delete_command_single_and_clear() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = fake_state(Some("sisa_abc"), version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;

    let (shutdown_tx, _ws, cache, _receipts, agent_task) = spawn_agent_cache(
        dir.path(),
        format!("http://{addr}"),
        Some("sisa_abc"),
        Duration::from_secs(60),
    );

    let seed_ws = tempfile::tempdir().expect("seed 工作区");
    std::fs::write(seed_ws.path().join("out"), b"x").expect("写");
    cache
        .save(
            "pipe-a",
            &sisyphus_proto::agent::CacheSpec {
                key: "k1".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            seed_ws.path(),
        )
        .await;
    cache
        .save(
            "pipe-b",
            &sisyphus_proto::agent::CacheSpec {
                key: "k2".into(),
                paths: vec!["out".into()],
                files: vec![],
            },
            seed_ws.path(),
        )
        .await;
    let k1_dir = cache.root().join("pipe-a").join("k1");
    let k2_dir = cache.root().join("pipe-b").join("k2");
    wait_until(|| async { k1_dir.exists() && k2_dir.exists() }).await;

    let tx = {
        wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
        state.last_session_tx()
    };

    // 单 key 删除 k1（点名真完整 key）。
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::CacheCmd(CacheCommand {
            kind: Some(CacheKind::Delete(CacheDeleteRequest { key: "k1".into() })),
        })),
    }))
    .await
    .expect("下发单 key 删除");
    wait_until(|| async { !k1_dir.exists() }).await;
    assert!(k2_dir.exists(), "k2 保留");

    // 全清（key 空）。
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::CacheCmd(CacheCommand {
            kind: Some(CacheKind::Delete(CacheDeleteRequest { key: String::new() })),
        })),
    }))
    .await
    .expect("下发全清");
    wait_until(|| async { !k2_dir.exists() }).await;
    assert_eq!(cache.cache_bytes(), 0, "全清后 registry 空");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

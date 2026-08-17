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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sisyphus_agent::channel::{Backoff, ChannelConfig, PlatformDiskSampler, PlatformLabels};
use sisyphus_agent::config::{self, Overrides};
use sisyphus_agent::Agent;
use sisyphus_proto::agent::{
    CacheCommand, ChannelMessage, Handshake, JobReported, UpgradeCommand, Version,
    WorkspaceCommand, agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind, cache_command::Kind as CacheKind, workspace_command::Kind as WorkspaceKind,
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
    /// 活动会话的下行发送器（测试注入下行指令用）。
    sessions: Mutex<Vec<mpsc::Sender<Result<ChannelMessage, Status>>>>,
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
    fn last_session_tx(&self) -> mpsc::Sender<Result<ChannelMessage, Status>> {
        self.sessions.lock().expect("锁").last().cloned().expect("应有会话")
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
            return Err(Status::unauthenticated("fake: Agent token 无效、缺失或已停用"));
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
        while let Some(msg) = inbound.message().await.map_err(|e| Status::internal(e.to_string()))?
        {
            if let Some(Kind::Handshake(h)) = msg.kind {
                agent_version = h.agent_version.clone();
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
                        agent_version: Some(state.server_version.clone()),
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
                        agent_version: Some(state.server_version.clone()),
                        agent_name: "fake-server".into(),
                    })),
                }))
                .await
                .is_err()
            {
                return;
            }
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.kind {
                    Some(Kind::Heartbeat(hb)) => state.heartbeats.lock().expect("锁").push(hb),
                    Some(Kind::JobReported(r)) => state.reported.lock().expect("锁").push(r),
                    _ => {}
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// 起 fake Server（真实 tonic，loopback socket），返回地址与 JoinHandle。
async fn spawn_fake(state: Arc<FakeState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
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
        sessions: Mutex::new(Vec::new()),
    })
}

// ============================================================
// agent 装配
// ============================================================

/// 注入短心跳/短退避（0 抖动）的通道配置——用例确定性，不依赖真实
/// 15s 心跳与 1s 退避。
fn channel_config(server_url: String, token: Option<&str>, data_dir: &Path) -> ChannelConfig {
    ChannelConfig {
        server_url,
        token: token.map(str::to_string),
        heartbeat_interval: Duration::from_millis(200),
        backoff: Backoff::with_params(Duration::from_millis(50), Duration::from_millis(300), 0.0),
        labels: Arc::new(PlatformLabels),
        disk: Arc::new(PlatformDiskSampler::new(data_dir.to_path_buf())),
    }
}

/// 组装组合根并 spawn `Agent::run`。返回（关闭发送端, 收帧观测, JoinHandle）。
fn spawn_agent(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
) -> (
    watch::Sender<bool>,
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
    let agent = Agent::with_channel_config(cfg, channel_config(server_url, token, data_dir));
    let receipts = agent.receipts();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, receipts, task)
}

/// 直接驱动单次连接（不经 Agent::run 重连循环）——认证/版本拒绝的断言面。
async fn connect_once(
    server_url: String,
    token: Option<&str>,
    data_dir: &Path,
) -> Result<(), sisyphus_agent::channel::ChannelError> {
    let cfg = channel_config(server_url, token, data_dir);
    let dispatch = dummy_dispatch();
    sisyphus_agent::channel::run_connection(&cfg, &dispatch, Arc::new(RwLock::new(Vec::new())))
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

    let (shutdown_tx, _receipts, agent_task) =
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
        "容器探测占位不置标签"
    );

    // 在途上报：连接建立即上报（本批为空集，机制在）。
    wait_until(|| async { !state.reported().is_empty() }).await;
    assert!(state.reported()[0].job_ids.is_empty(), "本批在途为空集");

    // 心跳：15s 语义经注入短间隔验证，附带真实磁盘占用（卷级 + 占位 0）。
    wait_until(|| async { !state.heartbeats().is_empty() }).await;
    let heartbeats = state.heartbeats();
    let disk = heartbeats.last().expect("心跳").disk.as_ref().expect("磁盘占用");
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
    assert!(!state.token_present().first().copied().unwrap_or(true), "缺 token 时不带凭据头");

    // 错 token：单次连接被拒。
    let err = connect_once(format!("http://{addr}"), Some("sisa_wrong"), dir.path()).await;
    assert!(err.is_err(), "错 token 应拒连");

    // Agent::run 层：错 token 下永久退避重连（attempts 持续增长），进程
    // 不自杀（run 任务仍在跑）；shutdown 干净退出。
    let before = state.attempts();
    let (shutdown_tx, _receipts, agent_task) =
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
    let (shutdown_tx, _receipts, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));
    wait_until(|| async { state.attempts() >= before + 2 }).await;
    assert!(!agent_task.is_finished(), "停用拒连不自杀：run 循环仍在重试");

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

    let (shutdown_tx, _receipts, agent_task) =
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

    let (shutdown_tx, receipts, agent_task) =
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

//! Spec B3 tracer bullet + 孤儿上报（票 B3-T10 / #55）。
//!
//! B2c b2c_tracer_bullet 的 Agent 侧镜像：一条全链路集成用例贯穿
//! 注册码换 token → 握手认证 → 下发 shell+checkout+缓存声明 JobSpec → ack →
//! 真实执行 → 日志 seq 流回 → 断连 → 续跑落缓冲 → 重连幂等补传 → 工作区/缓存
//! 指令往返 → 升级（排空→下载→校验→换入→spawn 断言）→ 终态。loopback 真实
//! tonic ↔ 真实 [`sisyphus_agent::Agent`] 组合根，runner 跑真实子进程、
//! checkout 跑真实 git、workspace/cache/logbuf 全用 tempfile 真实文件，不起
//! 独立进程（B2c / B3-T1 同纪律）。
//!
//! 第二条用例覆盖「孤儿上报」：Agent 重启（新组合根、同 data dir、空在途集）
//! → 通道检测孤儿缓冲 → 补传日志 → 删除缓冲 → `JobReported(job_ids=[])` 不
//! 认领孤儿（执行丢弃、日志保留作取证后再清，ADR-0008/0013）。
//!
//! 断言面全是经通道帧的 fake Server 观测 + 文件系统结果 + spawn 构造点，不停靠
//! agent 内部循环细节（B2c / B3-T1 同纪律）。异步路径用 `wait_until` 轮询驱动
//! 避免 flaky。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use sisyphus_agent::Agent;
use sisyphus_agent::channel::{Backoff, ChannelConfig, PlatformDiskSampler, StaticLabels};
use sisyphus_agent::config::{self, Overrides};
use sisyphus_agent::register::{persist_token, register};
use sisyphus_agent::upgrader::{DownloadError, Downloader, SpawnFailure, Spawner, UpgradeDeps};
use sisyphus_agent::workspace::Workspace;
use sisyphus_proto::agent::{
    CacheCommand, CacheSpec, ChannelMessage, CheckoutStep, Handshake, JobAck, JobPhase,
    JobReported, JobSpec, JobStatus, JobStep, ShellStep, UpgradeCommand, UpgradePhase, Version,
    WorkspaceCommand, WorkspaceListRequest,
    agent_channel_server::{AgentChannel, AgentChannelServer},
    cache_command::Kind as CacheKind,
    channel_message::Kind,
    job_step::Kind as StepKind,
    workspace_command::Kind as WorkspaceKind,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

/// 等到谓词成立或超时（异步轮询，15s 上限，避免 flaky——CI Windows
/// runner 上 spawn pwsh + gRPC 回传链路抖动需更宽上限）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..150 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("条件在 15s 内未成立");
}

/// sha256 → 小写 hex（与 upgrader 内部同算法）。
fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// 与 workspace 同版本（兼容窗口内）。
fn version(major: u32, minor: u32, patch: u32) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

// ============================================================
// fake HTTP register stub（注册码换 token 的 HTTP 缝）
// ============================================================

/// 极简 HTTP/1.1 stub：单连接读请求行 + 头 + body → 按配定 token 回 200。
/// 与 register_http.rs 同款（dev-deps 手写对端，不依赖 server crate）。
struct RegisterStub {
    addr: std::net::SocketAddr,
}

impl RegisterStub {
    /// 起 stub：每连回固定 token（200 + JSON）。`Connection: close` 短连。
    fn spawn(token: &str) -> Self {
        let body = format!(r#"{{"token":"{token}"}}"#);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let response = response.as_bytes().to_vec();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut content_length = 0usize;
                let mut line = String::new();
                // 请求行（读后弃）。
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header == "\r\n" || header == "\n"
                    {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                let _ = stream.write_all(&response);
            }
        });
        Self { addr }
    }
}

// ============================================================
// fake gRPC Server（认证 + 闸门 + 收集 + 下行注入）
// ============================================================

/// fake Server 的可观测状态与行为旋钮（Arc 共享：测试与 service 同持）。
struct FakeState {
    /// 期望的 Agent token（认证校验）。
    expect_token: String,
    /// 回发的 Server 版本。
    server_version: Version,
    /// 连接闸门：关闭时 `connect` 立即返回 Err（Agent 退避重试，不握手/不补传）。
    /// 用于断线补传相位——关闸期间 Agent 续跑、日志落缓冲，开闸后重连补传。
    gate: AtomicBool,
    /// 每次连接的 Agent 握手（名 + 版本）。
    handshakes: Mutex<Vec<(String, Version)>>,
    /// 每次连接是否带 authorization 头。
    token_present: Mutex<Vec<bool>>,
    /// 收到的在途上报（JobReported）。
    reported: Mutex<Vec<JobReported>>,
    /// 收到的日志帧（LogBatch）。
    log_batches: Mutex<Vec<sisyphus_proto::agent::LogBatch>>,
    /// 收到的任务回执（JobAck）。
    acks: Mutex<Vec<JobAck>>,
    /// 收到的任务状态（JobStatus）。
    statuses: Mutex<Vec<JobStatus>>,
    /// 收到的升级阶段（UpgradeStatus）。
    upgrade_statuses: Mutex<Vec<sisyphus_proto::agent::UpgradeStatus>>,
    /// 收到的工作区列表响应（WorkspaceList）。
    workspace_lists: Mutex<Vec<sisyphus_proto::agent::WorkspaceList>>,
    /// 收到的缓存列表响应（CacheList）。
    cache_lists: Mutex<Vec<sisyphus_proto::agent::CacheList>>,
    /// 活动会话的下行发送器（测试注入下行指令用）。
    sessions: Mutex<Vec<mpsc::Sender<Result<ChannelMessage, Status>>>>,
    /// 断开信号（备用；主用 gate + drop_after_handshake）。
    drop_signal: watch::Sender<bool>,
}

impl FakeState {
    fn handshakes(&self) -> Vec<(String, Version)> {
        self.handshakes.lock().expect("锁").clone()
    }
    fn token_present(&self) -> Vec<bool> {
        self.token_present.lock().expect("锁").clone()
    }
    fn reported(&self) -> Vec<JobReported> {
        self.reported.lock().expect("锁").clone()
    }
    fn log_batches(&self) -> Vec<sisyphus_proto::agent::LogBatch> {
        self.log_batches.lock().expect("锁").clone()
    }
    fn acks(&self) -> Vec<JobAck> {
        self.acks.lock().expect("锁").clone()
    }
    fn statuses(&self) -> Vec<JobStatus> {
        self.statuses.lock().expect("锁").clone()
    }
    fn upgrade_statuses(&self) -> Vec<sisyphus_proto::agent::UpgradeStatus> {
        self.upgrade_statuses.lock().expect("锁").clone()
    }
    fn workspace_lists(&self) -> Vec<sisyphus_proto::agent::WorkspaceList> {
        self.workspace_lists.lock().expect("锁").clone()
    }
    fn cache_lists(&self) -> Vec<sisyphus_proto::agent::CacheList> {
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
    /// 关闸：新连接一律拒绝（Agent 退避重试、不握手不补传）。
    fn close_gate(&self) {
        self.gate.store(false, Ordering::SeqCst);
    }
    /// 开闸：下一轮重连进入稳定会话。
    fn open_gate(&self) {
        self.gate.store(true, Ordering::SeqCst);
    }
    /// 断开全部活动会话（模拟 Server 掉线）。
    fn drop_all_sessions(&self) {
        let _ = self.drop_signal.send(true);
        self.sessions.lock().expect("锁").clear();
    }
}

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

        // 闸门：关闭时拒绝（Agent 退避重试、不握手不补传——断线补传相位的
        // 离线窗口由它制造）。
        if !state.gate.load(Ordering::SeqCst) {
            return Err(Status::unavailable("fake: 闸门关闭（模拟离线）"));
        }

        // 认证：校验 `Authorization: Bearer <token>`。
        let auth = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim)
            .map(str::to_string);
        state.token_present.lock().expect("锁").push(auth.is_some());
        if auth.as_deref() != Some(state.expect_token.as_str()) {
            return Err(Status::unauthenticated("fake: Agent token 无效或缺失"));
        }

        // 首帧必须是握手。
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
        state.sessions.lock().expect("锁").push(tx.clone());

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
            let mut drop_rx = state.drop_signal.subscribe();
            loop {
                tokio::select! {
                                _ = drop_rx.changed() => {
                    // 强制断连：经 downlink 显式送一帧 Status 错误（而非依赖 END_STREAM）。
                    // tonic/hyper 客户端连接任务在上行有未确认数据时可能不处理 downlink 的
                    // END_STREAM；显式 Status 错误帧使 agent 的 inbound.message() 返 Err，
                    // read_and_dispatch 即返 → run_connection 退避重连（与生产面真实网络
                    // 断连同形：TCP 错误即返）。
                    let _ = tx.send(Err(Status::internal("fake: 模拟断连"))).await;
                    break
                }
                                msg = inbound.message() => {
                                    let Ok(Some(msg)) = msg else { break };
                                    match msg.kind {
                                        Some(Kind::JobReported(r)) => state.reported.lock().expect("锁").push(r),
                                        Some(Kind::LogBatch(b)) => state.log_batches.lock().expect("锁").push(b),
                                        Some(Kind::JobAck(a)) => state.acks.lock().expect("锁").push(a),
                                        Some(Kind::JobStatus(s)) => state.statuses.lock().expect("锁").push(s),
                                        Some(Kind::UpgradeStatus(u)) => state.upgrade_statuses.lock().expect("锁").push(u),
                                        Some(Kind::WorkspaceList(l)) => state.workspace_lists.lock().expect("锁").push(l),
                                        Some(Kind::CacheList(l)) => state.cache_lists.lock().expect("锁").push(l),
                                        _ => {}
                                    }
                                }
                            }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// 起 fake Server（真实 tonic，loopback socket）。
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

fn fake_state(token: &str, server_version: Version) -> Arc<FakeState> {
    Arc::new(FakeState {
        expect_token: token.to_string(),
        server_version,
        gate: AtomicBool::new(true),
        handshakes: Mutex::new(Vec::new()),
        token_present: Mutex::new(Vec::new()),
        reported: Mutex::new(Vec::new()),
        log_batches: Mutex::new(Vec::new()),
        acks: Mutex::new(Vec::new()),
        statuses: Mutex::new(Vec::new()),
        upgrade_statuses: Mutex::new(Vec::new()),
        workspace_lists: Mutex::new(Vec::new()),
        cache_lists: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
        drop_signal: watch::channel(false).0,
    })
}

// ============================================================
// agent 装配（注入短心跳/短退避/fake 升级依赖）
// ============================================================

/// 注入短心跳/短退避（0 抖动）的通道配置——确定性，不依赖真实 15s 心跳与 1s 退避。
fn channel_config(server_url: String, token: Option<&str>, data_dir: &Path) -> ChannelConfig {
    ChannelConfig {
        server_url,
        token: token.map(str::to_string),
        heartbeat_interval: Duration::from_millis(500),
        backoff: Backoff::with_params(Duration::from_millis(50), Duration::from_millis(300), 0.0),
        labels: Arc::new(StaticLabels),
        disk: Arc::new(PlatformDiskSampler::new(data_dir.to_path_buf())),
        workspace_sample_interval: Duration::from_secs(3600),
    }
}

/// 装配工作区共享状态 + 低频采样器（挂 usage），与 `Agent::new` 同款。
fn build_workspace(
    cfg: &config::Config,
) -> (
    Workspace,
    std::sync::Arc<sisyphus_agent::workspace::WorkspaceSampler>,
) {
    let root = cfg.workspaces_dir();
    let sampler = std::sync::Arc::new(sisyphus_agent::workspace::WorkspaceSampler::new(
        root.clone(),
    ));
    let state = Workspace::new(root).with_usage(sampler.clone());
    (state, sampler)
}

/// 装配缓存共享状态（ADR-0012），与 `Agent::new` 同款。
fn build_cache(cfg: &config::Config) -> sisyphus_agent::cache::Cache {
    sisyphus_agent::cache::Cache::new(cfg.cache_dir(), cfg.cache_capacity_bytes())
}

/// 组装组合根并 spawn `Agent::run`。`token` 注入通道配置（tracer bullet 经
/// 注册码换得后注入；孤儿用例直注）。返回（关闭端, 工作区状态, 缓存状态, 任务）。
fn spawn_agent(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
    upgrade_deps: UpgradeDeps,
) -> (
    watch::Sender<bool>,
    Workspace,
    sisyphus_agent::cache::Cache,
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
        ws_state.clone(),
        ws_sampler,
        cache_state.clone(),
    )
    .with_upgrader_deps(upgrade_deps);
    let ws = agent.workspace_state();
    let cache = agent.cache_state();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, ws, cache, task)
}

// ============================================================
// fake 升级依赖（下载器/启动器，不真下载/真重启进程）
// ============================================================

/// 下载器观测：记录收到的 (url, token)。
type DlSeen = Arc<Mutex<Vec<(String, Option<String>)>>>;
/// 启动器观测：记录被 spawn 的二进制路径。
type SpawnSeen = Arc<Mutex<Vec<PathBuf>>>;

struct FakeDownloader {
    bytes: Result<Vec<u8>, String>,
    seen: DlSeen,
}
#[tonic::async_trait]
impl Downloader for FakeDownloader {
    async fn download(&self, url: &str, token: Option<&str>) -> Result<Vec<u8>, DownloadError> {
        self.seen
            .lock()
            .expect("锁")
            .push((url.to_string(), token.map(str::to_string)));
        self.bytes.clone().map_err(DownloadError)
    }
}

struct FakeSpawner {
    results: Vec<Result<(), String>>,
    next: Arc<Mutex<usize>>,
    recorded: SpawnSeen,
}
#[tonic::async_trait]
impl Spawner for FakeSpawner {
    async fn spawn(&self, bin: &Path, _args: Vec<String>) -> Result<(), SpawnFailure> {
        self.recorded.lock().expect("锁").push(bin.to_path_buf());
        let mut idx = self.next.lock().expect("锁");
        let i = *idx;
        *idx += 1;
        match self.results.get(i) {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(SpawnFailure(e.clone())),
            None => Err(SpawnFailure("fake: 结果序列耗尽".into())),
        }
    }
}

/// 升级依赖包构造（下载器/启动器/当前二进制路径）。当前二进制写 `OLD`。
fn upgrade_deps(
    bin: &Path,
    dl_bytes: Result<Vec<u8>, String>,
    spawn_results: Vec<Result<(), String>>,
    dl_seen: &DlSeen,
    spawn_seen: &SpawnSeen,
) -> UpgradeDeps {
    std::fs::write(bin, b"OLD").expect("写旧二进制");
    let downloader: Arc<dyn Downloader> = Arc::new(FakeDownloader {
        bytes: dl_bytes,
        seen: dl_seen.clone(),
    });
    let spawner: Arc<dyn Spawner> = Arc::new(FakeSpawner {
        results: spawn_results,
        next: Arc::new(Mutex::new(0)),
        recorded: spawn_seen.clone(),
    });
    UpgradeDeps {
        downloader,
        spawner,
        current_exe: bin.to_path_buf(),
    }
}

// ============================================================
// 本地 git 仓库（checkout 步骤真实检出用）
// ============================================================

/// 创建本地 git 仓库并返回其绝对路径 + HEAD commit sha（含一个提交文件 hello.txt = "v1\n"）。
fn local_git_repo(parent: &Path, name: &str) -> (PathBuf, String) {
    let repo = parent.join(name);
    std::fs::create_dir_all(&repo).expect("建 repo 目录");
    let git = |args: &[&str]| {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} 失败：{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    git(&["init", "--quiet"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&["config", "user.email", "test@sisyphus.local"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("hello.txt"), "v1\n").expect("写文件");
    git(&["add", "hello.txt"]);
    git(&["commit", "--quiet", "-m", "v1"]);
    let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    (repo, sha)
}

/// 平台默认解释器能跑的「长睡眠」命令（断线补传相位跨离线窗口用）。
fn sleep_cmd() -> String {
    if cfg!(unix) {
        "sleep 1".to_string()
    } else {
        // Windows：ping -n 2 睡约 1s（pwsh/cmd 均可）。
        "ping -n 2 127.0.0.1".to_string()
    }
}

/// 构造 tracer bullet 的 JobSpec：checkout + sleep（跨离线窗口）+ 写缓存文件 + 输出 "buffered"。
/// 缓存声明 paths=["cacheable"]（save 仅成功后；restore 在末个 checkout 后即 step 1 前）。
fn tracer_spec(job_id: &str, repo_url: &str, sha: &str, caches: Vec<CacheSpec>) -> ChannelMessage {
    let write_cacheable = if cfg!(unix) {
        "echo content > cacheable".to_string()
    } else {
        // pwsh/cmd 都认 `echo x > file` 重定向。
        "echo content > cacheable".to_string()
    };
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: "tracer-pipe".into(),
            job_name: "tracer-job".into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![
                JobStep {
                    name: "checkout".into(),
                    seq: 0,
                    kind: Some(StepKind::Checkout(CheckoutStep {
                        vcs: sisyphus_proto::agent::VcsType::VcsGit as i32,
                        repo_url: repo_url.into(),
                        r#ref: "main".into(),
                        commit: sha.into(),
                        submodules: false,
                    })),
                },
                JobStep {
                    name: "sleep".into(),
                    seq: 1,
                    kind: Some(StepKind::Shell(ShellStep {
                        command: sleep_cmd(),
                    })),
                },
                JobStep {
                    name: "write-cacheable".into(),
                    seq: 2,
                    kind: Some(StepKind::Shell(ShellStep {
                        command: write_cacheable,
                    })),
                },
                JobStep {
                    name: "echo-buffered".into(),
                    seq: 3,
                    kind: Some(StepKind::Shell(ShellStep {
                        command: "echo buffered".into(),
                    })),
                },
            ],
            env: HashMap::new(),
            exec_env: None,
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches,
            secrets: vec![],
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// 某 job 的全部输出字节（stdout + stderr 合流，按 seq 序）。
fn output_bytes(state: &FakeState, job_id: &str) -> Vec<u8> {
    let owned = state.log_batches();
    let mut batches: Vec<&sisyphus_proto::agent::LogBatch> =
        owned.iter().filter(|b| b.job_id == job_id).collect();
    batches.sort_by_key(|b| b.start_seq);
    let mut out = Vec::new();
    for b in batches {
        for ev in &b.events {
            if let Some(sisyphus_proto::agent::log_event::Kind::Output(o)) = &ev.kind {
                out.extend_from_slice(&o.data);
            }
        }
    }
    out
}

/// 某 job 的全部 seq（去重排序，补传幂等断言用）。
fn seqs_seen(state: &FakeState, job_id: &str) -> Vec<u64> {
    let mut seen: Vec<u64> = state
        .log_batches()
        .iter()
        .filter(|b| b.job_id == job_id)
        .flat_map(|b| b.events.iter().map(|e| e.seq))
        .collect();
    seen.sort_unstable();
    seen.dedup();
    seen
}

/// 某 job 是否已上报终态（phase >= Succeeded）。
fn terminal_status(state: &FakeState, job_id: &str) -> Option<JobStatus> {
    state.statuses().into_iter().find(|s| {
        s.job_id == job_id
            && !matches!(
                s.phase(),
                JobPhase::JobUnspecified | JobPhase::JobRunning | JobPhase::JobUnknown
            )
    })
}

/// 当前阶段是否已上报（含 error 子串匹配，空串 = 任意）。
fn has_upgrade_phase(state: &Arc<FakeState>, phase: UpgradePhase, contains: &str) -> bool {
    state
        .upgrade_statuses()
        .iter()
        .any(|u| u.phase == phase as i32 && (contains.is_empty() || u.error.contains(contains)))
}

// ============================================================
// 用例 1：B3 tracer bullet 全链路
// ============================================================

/// Spec B3 tracer bullet：注册码换 token → 认证 → 下发 shell+checkout+缓存声明
/// JobSpec → ack → 真实执行 → 日志 seq 流回 → 断连 → 续跑落缓冲 → 重连幂等补传 →
/// 工作区/缓存指令往返 → 升级（排空→下载→校验→换入→spawn）→ 终态。一条用例贯穿
/// B3 全链路（B2c b2c_tracer_bullet 的 Agent 侧镜像）。
#[tokio::test]
async fn b3_tracer_bullet_full_chain() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let (repo, sha) = local_git_repo(dir.path(), "src-repo");

    // === 相位 1：注册码换 token（HTTP 缝） ===
    // fake HTTP register stub 回固定 token；真 reqwest client 驱动 register::register。
    let token = "sisa_tracer_bullet";
    let reg_stub = RegisterStub::spawn(token);
    let reg_client = reqwest::Client::new();
    let obtained = register(
        &reg_client,
        &format!("http://{}", reg_stub.addr),
        "tracer-host",
        "sisa_reg_tracer",
    )
    .await
    .expect("兑码成功");
    assert_eq!(obtained, token, "兑码换得 Server 签发的 per-Agent token");
    // token 落盘（与 bin 的 bootstrap_register 同路径）+ 读回（Agent::new 的取凭据缝）。
    persist_token(dir.path(), token).expect("落盘");
    assert_eq!(
        config::read_token(dir.path()).as_deref(),
        Some(token),
        "落盘 token 即后续直连凭据"
    );

    // === 相位 2：装配组合根 + 升级依赖 ===
    let bin_dir = tempfile::tempdir().expect("临时二进制目录");
    let bin = bin_dir.path().join("agent.bin");
    let new_bytes = b"NEW-BIN".to_vec();
    let new_sha = sha256_hex(&new_bytes);
    let dl_seen: DlSeen = Arc::new(Mutex::new(Vec::new()));
    let spawn_seen: SpawnSeen = Arc::new(Mutex::new(Vec::new()));
    let deps = upgrade_deps(&bin, Ok(new_bytes), vec![Ok(())], &dl_seen, &spawn_seen);

    let state = fake_state(token, version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, cache, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some(token), deps);

    // === 相位 3：握手认证（经注册换得的 token） ===
    wait_until(|| async { !state.handshakes().is_empty() }).await;
    let (name, v) = &state.handshakes()[0];
    assert!(!name.is_empty(), "握手携带主机名");
    assert_eq!(v, &version(1, 0, 0), "握手携带 Agent 版本");
    assert!(
        state.token_present()[0],
        "token 随连接呈送（注册换得的 token 认证通过）"
    );

    // === 相位 4：下发 shell+checkout+缓存声明 JobSpec → ack → 真实执行 → 日志流回 ===
    let caches = vec![CacheSpec {
        key: "tracer-cache".into(),
        paths: vec!["cacheable".into()],
        files: vec![],
    }];
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    let tx = state.last_session_tx();
    tx.send(Ok(tracer_spec(
        "tracer-job",
        &repo.to_string_lossy(),
        &sha,
        caches,
    )))
    .await
    .expect("下发 JobSpec");
    // ack（accept）。
    wait_until(|| async {
        state
            .acks()
            .iter()
            .any(|a| a.job_id == "tracer-job" && a.accepted)
    })
    .await;
    // 日志 seq 流回：checkout 步骤的 start 事件经通道到达（活体转发，断连前）。
    wait_until(|| async {
        state
            .log_batches()
            .iter()
            .any(|b| b.job_id == "tracer-job" && !b.events.is_empty())
    })
    .await;

    // === 相位 5：断连 → 续跑 → 日志落缓冲 ===
    // 关闸 + 强制断当前会话（downlink 送 Err 帧——模拟真实断连：agent 的
    // inbound.message() 返 Err 即走清理退避重连；不依赖 downlink END_STREAM，因
    // tonic/hyper 客户端连接任务在上行有未确认数据时可能不处理 END_STREAM）。
    // 期间 job 续跑（checkout 完成 + sleep + 写 cacheable + echo buffered），日志
    // 落盘缓冲、终态缓冲到 uplink pending。
    state.close_gate();
    state.drop_all_sessions();
    // 等 job 续跑至写出 cacheable 文件（step 2 完成）。
    let cacheable = ws
        .resolve("tracer-pipe", "tracer-job")
        .expect("resolve 工作区")
        .join("cacheable");
    wait_until(|| async { cacheable.is_file() }).await;
    // step 2 完成后 step 3（echo buffered）与终态几乎瞬时；留 800ms 确保它们在离线
    // 窗口内完成并落缓冲（非活体）。
    tokio::time::sleep(Duration::from_millis(800)).await;

    // === 相位 6：重连 → 幂等补传 → 终态补发 ===
    state.open_gate();
    // 重连后补传：job 的全部日志（含离线期间缓冲的 "buffered"）按 seq 幂等重放；
    // 离线期间缓冲的终态（succeeded）经 uplink flush_pending 补发。
    wait_until(|| async { state.handshakes().len() >= 2 }).await;
    wait_until(|| async {
        String::from_utf8_lossy(&output_bytes(&state, "tracer-job")).contains("buffered")
    })
    .await;
    wait_until(|| async { terminal_status(&state, "tracer-job").is_some() }).await;
    let terminal = terminal_status(&state, "tracer-job").expect("终态");
    assert_eq!(
        terminal.phase(),
        JobPhase::JobSucceeded,
        "续跑至成功（离线缓冲的终态经重连补发）：{}",
        terminal.detail
    );
    // 补传幂等：全部 seq 到达、无缺（不丢、不越界）。
    let seen = seqs_seen(&state, "tracer-job");
    assert!(
        !seen.is_empty() && *seen.first().unwrap() == 0,
        "seq 从 0 起：{seen:?}"
    );
    // checkout 真实检出落盘：工作区有 hello.txt（断连期间完成、重连后可见）。
    let ws_dir = ws
        .resolve("tracer-pipe", "tracer-job")
        .expect("resolve 工作区");
    assert!(
        ws_dir.join("hello.txt").is_file(),
        "checkout 检出 hello.txt"
    );
    // 缓存 save 仅成功后：cacheable 已 save 到缓存目录。
    let cache_dir = cache.root().join("tracer-pipe").join("tracer-cache");
    wait_until(|| async { cache_dir.join("cacheable").is_file() }).await;

    // === 相位 7：工作区/缓存指令往返 ===
    let tx = state.last_session_tx();
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::WorkspaceCmd(WorkspaceCommand {
            kind: Some(WorkspaceKind::List(WorkspaceListRequest {})),
        })),
    }))
    .await
    .expect("下发工作区列表指令");
    wait_until(|| async { !state.workspace_lists().is_empty() }).await;
    let ws_list = &state.workspace_lists()[0];
    assert!(
        ws_list
            .entries
            .iter()
            .any(|e| e.pipeline == "tracer-pipe" && e.job == "tracer-job"),
        "工作区列表含 tracer-pipe/tracer-job：{:?}",
        ws_list.entries
    );

    let tx = state.last_session_tx();
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::CacheCmd(CacheCommand {
            kind: Some(CacheKind::List(Default::default())),
        })),
    }))
    .await
    .expect("下发缓存列表指令");
    wait_until(|| async { !state.cache_lists().is_empty() }).await;
    let cache_list = &state.cache_lists()[0];
    assert!(
        cache_list
            .entries
            .iter()
            .any(|e| e.pipeline == "tracer-pipe" && e.key.contains("tracer-cache")),
        "缓存列表含 tracer-cache 条目：{:?}",
        cache_list.entries
    );

    // === 相位 8：升级（排空 → 下载 → 校验 → 换入 → spawn 断言） ===
    // job 已终态（在途空）→ 排空即完成 → 下载（fake）→ sha256 校验 → 原子换入 →
    // spawn 新进程 → 旧进程退出。
    let tx = state.last_session_tx();
    tx.send(Ok(ChannelMessage {
        kind: Some(Kind::Upgrade(UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: new_sha,
            download_url: "http://get/pkg".into(),
        })),
    }))
    .await
    .expect("下发升级指令");
    wait_until(|| async { has_upgrade_phase(&state, UpgradePhase::UpgradeDraining, "") }).await;
    wait_until(|| async { has_upgrade_phase(&state, UpgradePhase::UpgradeDownloading, "") }).await;
    wait_until(|| async { has_upgrade_phase(&state, UpgradePhase::UpgradeSwapping, "") }).await;
    wait_until(|| async { has_upgrade_phase(&state, UpgradePhase::UpgradeRestarting, "") }).await;
    // spawn 构造点：收到的是换入后的当前路径（新二进制）。
    wait_until(|| async { !spawn_seen.lock().expect("锁").is_empty() }).await;
    assert_eq!(
        spawn_seen.lock().expect("锁").clone(),
        vec![bin.clone()],
        "spawn 的是换入后的新路径二进制"
    );
    assert_eq!(std::fs::read(&bin).unwrap(), b"NEW-BIN", "当前已换新");
    assert_eq!(
        std::fs::read(format!("{}.old", bin.display())).unwrap(),
        b"OLD",
        ".old 保留旧"
    );
    // 下载器收到 Bearer token（注册换得的同一 token）+ 绝对 URL。
    let dl = dl_seen.lock().expect("锁").clone();
    assert_eq!(dl.len(), 1);
    assert_eq!(dl[0].0, "http://get/pkg");
    assert_eq!(dl[0].1.as_deref(), Some(token));

    // === 相位 9：终态——旧进程退出（升级 spawn 置位 exit 信号） ===
    wait_until(|| async { agent_task.is_finished() }).await;

    let _ = shutdown_tx;
    server_task.abort();
}

// ============================================================
// 用例 2：孤儿上报（Agent 重启 → 孤儿缓冲补传 + 删除 + JobReported 空）
// ============================================================

/// Agent 重启后重连：孤儿缓冲（job 不在空在途集）补传后删除、`JobReported(job_ids=[])`
/// 不认领孤儿（执行丢弃、日志保留作取证后再清，ADR-0008/0013）。经真实 [`Agent::run`]
/// 重连路径（fresh 组合根、同 data dir、空在途集）。
#[tokio::test]
async fn orphan_backfill_after_agent_restart() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let data_dir = dir.path();

    // 模拟「前一个 Agent 实例跑过 orphan-job、落缓冲后崩溃」：用真实 LogBuffer 写几条
    // 事件到 `<data>/logbuf/orphan-job-0.jsonl`（production 代码、真实格式 + fsync）。
    let logbuf_dir = data_dir.join("logbuf");
    std::fs::create_dir_all(&logbuf_dir).expect("logbuf 目录");
    let prev_buf = sisyphus_agent::logbuf::LogBuffer::new(logbuf_dir, Duration::from_secs(60));
    let orphan_event = |data: &[u8]| sisyphus_proto::agent::LogEvent {
        seq: 0, // 缓冲层重编号
        kind: Some(sisyphus_proto::agent::log_event::Kind::Output(
            sisyphus_proto::agent::OutputChunk {
                stream: sisyphus_proto::agent::Stream::Stdout as i32,
                data: data.to_vec(),
            },
        )),
    };
    prev_buf
        .append("orphan-job", 0, orphan_event(b"orphan-line-1"))
        .await
        .expect("落盘");
    prev_buf
        .append("orphan-job", 0, orphan_event(b"orphan-line-2"))
        .await
        .expect("落盘");
    let orphan_path = prev_buf.path("orphan-job", 0);
    assert!(orphan_path.exists(), "孤儿缓冲落盘");
    drop(prev_buf); // 前一个实例「崩溃」：释放句柄，缓冲文件留作孤儿。

    // 「重启」：fresh 组合根、同 data dir、空在途集（内存态重启即丢）。
    let token = "sisa_orphan";
    let state = fake_state(token, version(1, 0, 0));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let deps = UpgradeDeps::safe_stub(); // 不触发升级
    let (shutdown_tx, _ws, _cache, agent_task) =
        spawn_agent(data_dir, format!("http://{addr}"), Some(token), deps);

    // 重连：握手 + 在途上报（空集——不认领孤儿）。
    wait_until(|| async { !state.handshakes().is_empty() }).await;
    wait_until(|| async { !state.reported().is_empty() }).await;
    let reported = &state.reported()[0];
    assert!(
        reported.job_ids.is_empty(),
        "重启后在途为空集——不认领孤儿（Server 据此判孤儿 aborted）：{:?}",
        reported.job_ids
    );

    // 孤儿缓冲补传：fake 收到 orphan-job 的日志（取证）。
    wait_until(|| async {
        state
            .log_batches()
            .iter()
            .any(|b| b.job_id == "orphan-job" && b.start_seq == 0)
    })
    .await;
    let orphan_out = output_bytes(&state, "orphan-job");
    assert!(
        String::from_utf8_lossy(&orphan_out).contains("orphan-line-1")
            && String::from_utf8_lossy(&orphan_out).contains("orphan-line-2"),
        "孤儿缓冲全量补传（取证）：{:?}",
        String::from_utf8_lossy(&orphan_out)
    );
    // 补传后删除孤儿缓冲（执行丢弃、日志保留作取证后清空）。
    wait_until(|| async { !orphan_path.exists() }).await;
    assert!(!orphan_path.exists(), "孤儿缓冲补传后删除");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

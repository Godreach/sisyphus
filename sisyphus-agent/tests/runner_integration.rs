//! 集成测试：runner host 后端全链路（票 B3-T5 / #59）。
//!
//! fake Server 下发真实 JobSpec / CancelBuild，收 JobAck / JobStatus / LogBatch；
//! agent 侧经 [`sisyphus_agent::Agent::with_channel_config`] 装配组合根并 spawn
//! `Agent::run`（与二进制同一组合根），runner 真实起 shell 进程、流式编码日志、
//! 上报终态。断言面全是经通道帧的 fake 观测，不停靠 agent 内部循环细节
//! （B2c / B3-T1 同纪律）。
//!
//! 覆盖验收（#59 AC）：
//! - JobSpec → ack → 真实执行 → 日志事件流 → 终态上报全链通；
//! - shell 默认解释器（Unix sh / Windows pwsh）+ cwd / env 正确；
//! - 机密注入 + 脱敏（输出 `***`，含跨输出块边界）+ 无匹配穿透；
//! - 日志编码（stdout/stderr stream 标记、step start/end、per-attempt 单调 seq、
//!   超限截断插标记不判败）；
//! - 取消：进程树终止、终态 cancelled；
//! - `${SISY_WORKSPACE}` 执行前替换；
//! - 同 job 去重（已在跑 → 拒收）。
//!
//! 超时终态映射（`Timeout → JobTimeout`）由 `runner::outcome_phase` 单测覆盖、
//! 进程树终止由 `exec` 单测覆盖——集成层 1 分钟超时下限对 CI 过慢，不在此跑。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sisyphus_agent::Agent;
use sisyphus_agent::channel::{Backoff, ChannelConfig, PlatformDiskSampler, StaticLabels};
use sisyphus_agent::config::{self, Overrides};
use sisyphus_agent::workspace::Workspace;
use sisyphus_proto::agent::{
    CancelBuild, ChannelMessage, CheckoutStep, ContainerEnv, ExecutionEnv, Handshake, JobAck,
    JobPhase, JobSpec, JobStatus, JobStep, LogBatch, ShellStep, Stream, VcsType, Version,
    agent_channel_server::{AgentChannel, AgentChannelServer},
    channel_message::Kind,
    execution_env::Kind as EnvKind,
    job_step::Kind as StepKind,
    log_event::Kind as EventKind,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

/// 等到谓词成立或超时（异步轮询，5s 上限，避免 flaky）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("条件在 5s 内未成立");
}

/// 平台默认解释器能跑的「退出码 N」命令（Unix sh / Windows pwsh+cmd 均认 `exit <code>`）。
fn exit_cmd(code: i32) -> String {
    format!("exit {code}")
}

/// 平台默认解释器能跑的「长睡眠」命令（取消/超时用例）。
fn sleep_cmd() -> String {
    if cfg!(unix) {
        "sleep 30".to_string()
    } else {
        // Windows：ping -n 30 睡约 29s（pwsh/cmd 均可）。
        "ping -n 30 127.0.0.1".to_string()
    }
}

// ============================================================
// fake Server
// ============================================================

struct RunnerState {
    expect_token: Option<String>,
    server_version: Version,
    acks: Mutex<Vec<JobAck>>,
    statuses: Mutex<Vec<JobStatus>>,
    log_batches: Mutex<Vec<LogBatch>>,
    sessions: Mutex<Vec<mpsc::Sender<Result<ChannelMessage, Status>>>>,
    drop_signal: watch::Sender<bool>,
}

impl RunnerState {
    fn acks(&self) -> Vec<JobAck> {
        self.acks.lock().expect("锁").clone()
    }
    fn statuses(&self) -> Vec<JobStatus> {
        self.statuses.lock().expect("锁").clone()
    }
    fn log_batches(&self) -> Vec<LogBatch> {
        self.log_batches.lock().expect("锁").clone()
    }
    fn last_session_tx(&self) -> mpsc::Sender<Result<ChannelMessage, Status>> {
        self.sessions
            .lock()
            .expect("锁")
            .last()
            .cloned()
            .expect("应有会话")
    }
}

struct RunnerServer {
    state: Arc<RunnerState>,
}

#[tonic::async_trait]
impl AgentChannel for RunnerServer {
    type ConnectStream = ReceiverStream<Result<ChannelMessage, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<ChannelMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let state = self.state.clone();
        // 认证（Bearer）。
        let auth = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::to_string);
        if auth.as_deref() != state.expect_token.as_deref() {
            return Err(Status::unauthenticated("fake: token 无效"));
        }

        let mut inbound = request.into_inner();
        // 首帧握手。
        let mut agent_version = None;
        while let Some(msg) = inbound
            .message()
            .await
            .map_err(|e| Status::internal(e.to_string()))?
        {
            if let Some(Kind::Handshake(h)) = msg.kind {
                agent_version = h.agent_version;
                break;
            }
        }
        let _ = agent_version;

        let (tx, rx) = mpsc::channel(64);
        state.sessions.lock().expect("锁").push(tx.clone());

        tokio::spawn(async move {
            // 回发握手。
            if tx
                .send(Ok(ChannelMessage {
                    kind: Some(Kind::Handshake(Handshake {
                        agent_version: Some(state.server_version),
                        agent_name: "fake-runner".into(),
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
                    _ = drop_rx.changed() => break,
                    msg = inbound.message() => {
                        let Ok(Some(msg)) = msg else { break };
                        match msg.kind {
                            Some(Kind::JobAck(a)) => state.acks.lock().expect("锁").push(a),
                            Some(Kind::JobStatus(s)) => state.statuses.lock().expect("锁").push(s),
                            Some(Kind::LogBatch(b)) => state.log_batches.lock().expect("锁").push(b),
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn spawn_fake(
    state: Arc<RunnerState>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = RunnerServer { state };
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentChannelServer::new(server))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });
    (addr, handle)
}

fn runner_state(token: Option<&str>) -> Arc<RunnerState> {
    Arc::new(RunnerState {
        expect_token: token.map(str::to_string),
        server_version: Version {
            major: 1,
            minor: 0,
            patch: 0,
        },
        acks: Mutex::new(Vec::new()),
        statuses: Mutex::new(Vec::new()),
        log_batches: Mutex::new(Vec::new()),
        sessions: Mutex::new(Vec::new()),
        drop_signal: watch::channel(false).0,
    })
}

// ============================================================
// agent 装配
// ============================================================

fn channel_cfg(server_url: String, token: Option<&str>, data_dir: &Path) -> ChannelConfig {
    ChannelConfig {
        server_url,
        token: token.map(str::to_string),
        heartbeat_interval: Duration::from_secs(3600), // 测试不依赖心跳
        backoff: Backoff::with_params(Duration::from_millis(50), Duration::from_millis(300), 0.0),
        labels: Arc::new(StaticLabels),
        disk: Arc::new(PlatformDiskSampler::new(data_dir.to_path_buf())),
        workspace_sample_interval: Duration::from_secs(3600),
    }
}

/// 装配组合根并 spawn `Agent::run`。返回（关闭端, 工作区状态, fake 状态, 任务）。
fn spawn_agent(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
) -> (watch::Sender<bool>, Workspace, tokio::task::JoinHandle<()>) {
    let cfg = config::Config::load(
        &Overrides {
            server_url: Some(server_url.clone()),
            data_dir: Some(data_dir.to_path_buf()),
            ..Overrides::default()
        },
        &Overrides::default(),
    )
    .expect("配置");
    let ws_root = cfg.workspaces_dir();
    let sampler = Arc::new(sisyphus_agent::workspace::WorkspaceSampler::new(
        ws_root.clone(),
    ));
    let ws_state = Workspace::new(ws_root).with_usage(sampler.clone());
    let cache_state = sisyphus_agent::cache::Cache::new(cfg.cache_dir(), cfg.cache_capacity_bytes());
    let agent = Agent::with_channel_config(
        cfg,
        channel_cfg(server_url, token, data_dir),
        ws_state.clone(),
        sampler,
        cache_state,
    );
    let ws = agent.workspace_state();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, ws, task)
}

/// 装配组合根并 spawn `Agent::run`，额外返回缓存共享状态（cache 集成测试用它
/// 预置/观察真实缓存目录）。与 `spawn_agent` 同款 + cache 状态。
fn spawn_agent_cache(
    data_dir: &Path,
    server_url: String,
    token: Option<&str>,
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
    let ws_root = cfg.workspaces_dir();
    let sampler = Arc::new(sisyphus_agent::workspace::WorkspaceSampler::new(ws_root.clone()));
    let ws_state = Workspace::new(ws_root).with_usage(sampler.clone());
    let cache_state = sisyphus_agent::cache::Cache::new(cfg.cache_dir(), cfg.cache_capacity_bytes());
    let agent = Agent::with_channel_config(
        cfg,
        channel_cfg(server_url, token, data_dir),
        ws_state.clone(),
        sampler,
        cache_state,
    );
    let ws = agent.workspace_state();
    let cache = agent.cache_state();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(agent.run(shutdown_rx));
    (shutdown_tx, ws, cache, task)
}

/// 构造一个 shell 步骤的 JobSpec。
fn shell_spec(
    job_id: &str,
    pipeline: &str,
    job: &str,
    command: &str,
    env: Vec<(&str, &str)>,
    secrets: Vec<&str>,
    log_limit: i64,
) -> ChannelMessage {
    let env: HashMap<String, String> = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: log_limit,
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Shell(ShellStep {
                    command: command.into(),
                })),
            }],
            env,
            exec_env: None,
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![],
            secrets: secrets.into_iter().map(str::to_string).collect(),
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// 构造一个 shell 步骤的 JobSpec，附带缓存声明（ADR-0012 集成测试用）。
fn shell_spec_with_caches(
    job_id: &str,
    pipeline: &str,
    job: &str,
    command: &str,
    caches: Vec<sisyphus_proto::agent::CacheSpec>,
) -> ChannelMessage {
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Shell(ShellStep {
                    command: command.into(),
                })),
            }],
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

/// 构造一个 checkout 步骤的 JobSpec（git，钉到 commit）。无凭据（本地 file 仓库）。
fn checkout_spec(
    job_id: &str,
    pipeline: &str,
    job: &str,
    url: &str,
    branch: &str,
    commit: &str,
    submodules: bool,
) -> ChannelMessage {
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Checkout(CheckoutStep {
                    vcs: VcsType::VcsGit as i32,
                    repo_url: url.into(),
                    r#ref: branch.into(),
                    commit: commit.into(),
                    submodules,
                })),
            }],
            env: HashMap::new(),
            exec_env: None,
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![],
            secrets: vec![],
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// 构造含 checkout + shell 两步的 JobSpec：先 checkout 钉到 commit，再跑 shell 命令。
fn checkout_then_shell_spec(
    job_id: &str,
    pipeline: &str,
    job: &str,
    url: &str,
    branch: &str,
    commit: &str,
    shell_cmd: &str,
) -> ChannelMessage {
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![
                JobStep {
                    name: "step-0".into(),
                    seq: 0,
                    kind: Some(StepKind::Checkout(CheckoutStep {
                        vcs: VcsType::VcsGit as i32,
                        repo_url: url.into(),
                        r#ref: branch.into(),
                        commit: commit.into(),
                        submodules: false,
                    })),
                },
                JobStep {
                    name: "step-1".into(),
                    seq: 1,
                    kind: Some(StepKind::Shell(ShellStep {
                        command: shell_cmd.into(),
                    })),
                },
            ],
            env: HashMap::new(),
            exec_env: None,
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![],
            secrets: vec![],
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// 创建本地 git 仓库并返回其绝对路径 + HEAD commit sha（含一个提交文件 hello.txt）。
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

/// 工作区目录里跑 `git rev-parse HEAD` 取当前 HEAD sha。
fn ws_head(ws: &Path) -> String {
    let out = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(ws)
        .output()
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "rev-parse 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// 向活动会话下发一帧。
async fn send_downlink(state: &RunnerState, msg: ChannelMessage) {
    wait_until(|| async { !state.sessions.lock().expect("锁").is_empty() }).await;
    state.last_session_tx().send(Ok(msg)).await.expect("下发");
}

/// 某 job 的全部输出字节（stdout + stderr 合流，按 seq 序）。
fn output_bytes(state: &RunnerState, job_id: &str) -> Vec<u8> {
    let owned = state.log_batches();
    let mut batches: Vec<&LogBatch> = owned.iter().filter(|b| b.job_id == job_id).collect();
    batches.sort_by_key(|b| b.start_seq);
    let mut out = Vec::new();
    for b in batches {
        for ev in &b.events {
            if let Some(EventKind::Output(o)) = &ev.kind {
                out.extend_from_slice(&o.data);
            }
        }
    }
    out
}

/// 某 job 的全部步骤事件（按 seq 序）。
fn step_events(state: &RunnerState, job_id: &str) -> Vec<sisyphus_proto::agent::StepEvent> {
    let owned = state.log_batches();
    let mut batches: Vec<&LogBatch> = owned.iter().filter(|b| b.job_id == job_id).collect();
    batches.sort_by_key(|b| b.start_seq);
    let mut out = Vec::new();
    for b in batches {
        for ev in &b.events {
            if let Some(EventKind::Step(s)) = &ev.kind {
                out.push(s.clone());
            }
        }
    }
    out
}

/// 某 job 是否收到 Truncated 标记事件。
fn has_truncated(state: &RunnerState, job_id: &str) -> bool {
    state.log_batches().iter().any(|b| {
        b.job_id == job_id
            && b.events
                .iter()
                .any(|ev| matches!(ev.kind.as_ref(), Some(EventKind::Truncated(_))))
    })
}

/// 某 job 的终态 JobStatus（phase >= Succeeded）。
fn terminal_status(state: &RunnerState, job_id: &str) -> Option<JobStatus> {
    state.statuses().into_iter().find(|s| {
        s.job_id == job_id
            && !matches!(
                s.phase(),
                JobPhase::JobUnspecified | JobPhase::JobRunning | JobPhase::JobUnknown
            )
    })
}

/// 等 fake 收到某 job 的 ack（accepted 与否）。
async fn await_ack(state: &RunnerState, job_id: &str, accepted: bool) -> JobAck {
    wait_until(|| async {
        state
            .acks()
            .iter()
            .any(|a| a.job_id == job_id && a.accepted == accepted)
    })
    .await;
    state
        .acks()
        .into_iter()
        .find(|a| a.job_id == job_id && a.accepted == accepted)
        .expect("ack")
}

/// 等 fake 收到某 job 的终态。
async fn await_terminal(state: &RunnerState, job_id: &str) -> JobStatus {
    wait_until(|| async { terminal_status(state, job_id).is_some() }).await;
    terminal_status(state, job_id).expect("终态")
}

// ============================================================
// 用例
// ============================================================

/// AC: JobSpec → ack(accept) → 真实执行 → 日志事件流 → 终态 succeeded 全链通。
/// 覆盖 shell 默认解释器、step start/end 事件、stdout stream 标记、per-attempt 单调 seq。
#[tokio::test]
async fn runs_shell_step_and_reports_success_with_step_events_and_seq() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        shell_spec("job-1", "pipe", "job", "echo hello", vec![], vec![], 0),
    )
    .await;

    let ack = await_ack(&state, "job-1", true).await;
    assert!(ack.error.is_empty(), "接受不应带 error：{}", ack.error);

    // running + succeeded 终态。
    wait_until(|| async {
        state
            .statuses()
            .iter()
            .any(|s| s.job_id == "job-1" && s.phase() == JobPhase::JobRunning)
    })
    .await;
    let terminal = await_terminal(&state, "job-1").await;
    assert_eq!(terminal.phase(), JobPhase::JobSucceeded);
    assert_eq!(terminal.exit_code, Some(0), "succeeded 退出码 0");

    // 日志：stdout 含 "hello"。
    wait_until(|| async {
        output_bytes(&state, "job-1")
            .windows(b"hello".len())
            .any(|w| w == b"hello")
    })
    .await;
    let out = output_bytes(&state, "job-1");
    assert!(
        String::from_utf8_lossy(&out).contains("hello"),
        "stdout 应含 hello：{out:?}"
    );

    // 步骤事件：start（含命令回显）+ end（exit_code Some(0)）。
    wait_until(|| async { step_events(&state, "job-1").len() >= 2 }).await;
    let steps = step_events(&state, "job-1");
    assert_eq!(steps.len(), 2, "start + end 两个事件");
    assert_eq!(steps[0].exit_code, None, "start 事件 exit_code=None");
    assert!(
        steps[0].command.contains("echo hello"),
        "start 携带命令回显"
    );
    assert_eq!(steps[1].exit_code, Some(0), "end 事件 exit_code=Some(0)");
    assert!(
        steps[1].step_ended_at_ms >= steps[1].step_started_at_ms,
        "end 不早于 start"
    );

    // per-attempt 单调 seq：所有事件 seq 连续无重复（按到达序）。
    let seqs: Vec<u64> = state
        .log_batches()
        .iter()
        .filter(|b| b.job_id == "job-1")
        .flat_map(|b| b.events.iter().map(|e| e.seq))
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(seqs.len(), sorted.len(), "seq 不重复：{seqs:?}");
    assert_eq!(sorted.first(), Some(&0), "seq 从 0 起");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: shell 步骤非零退出 → 终态 failed + 退出码。
#[tokio::test]
async fn shell_step_failure_reports_failed_with_exit_code() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        shell_spec("job-2", "pipe", "job", &exit_cmd(7), vec![], vec![], 0),
    )
    .await;
    await_ack(&state, "job-2", true).await;
    let terminal = await_terminal(&state, "job-2").await;
    assert_eq!(terminal.phase(), JobPhase::JobFailed, "非零退出 → failed");
    assert_eq!(terminal.exit_code, Some(7), "携带退出码 7");
    assert!(
        terminal.detail.contains('7'),
        "detail 点名退出码：{}",
        terminal.detail
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: cwd / env 正确 + `${SISY_WORKSPACE}` 执行前替换为 job 工作区绝对路径。
#[tokio::test]
async fn shell_step_cwd_env_and_sisy_workspace_correct() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 预解析 runner 将使用的工作区目录（同 pipeline/job 名 → 同一目录）。
    let ws_dir = ws.resolve("pipe", "job").expect("预解析工作区");
    let probe = ws_dir.join("probe.txt");

    // 三步：(0) 打印 env MY_VAR；(1) 在 cwd 写一个探针文件（证 cwd = 工作区）；
    // (2) 打印 ${SISY_WORKSPACE}（runner 执行前替换为工作区绝对路径）。
    let (cmd0, cmd1, cmd2) = if cfg!(unix) {
        (
            "echo \"$MY_VAR\"".to_string(),
            "echo marker > probe.txt".to_string(),
            "echo \"${SISY_WORKSPACE}\"".to_string(),
        )
    } else {
        // pwsh：$env:MY_VAR + 重定向写探针 + ${SISY_WORKSPACE}（runner 替换为路径）。
        (
            "echo $env:MY_VAR".to_string(),
            "echo marker > probe.txt".to_string(),
            "echo \"${SISY_WORKSPACE}\"".to_string(),
        )
    };
    let spec = ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: "job-3".into(),
            pipeline_name: "pipe".into(),
            job_name: "job".into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![
                JobStep {
                    name: "step-0".into(),
                    seq: 0,
                    kind: Some(StepKind::Shell(ShellStep { command: cmd0 })),
                },
                JobStep {
                    name: "step-1".into(),
                    seq: 1,
                    kind: Some(StepKind::Shell(ShellStep { command: cmd1 })),
                },
                JobStep {
                    name: "step-2".into(),
                    seq: 2,
                    kind: Some(StepKind::Shell(ShellStep { command: cmd2 })),
                },
            ],
            env: HashMap::from([("MY_VAR".into(), "hello-cwd".into())]),
            ..Default::default()
        }))),
    };
    send_downlink(&state, spec).await;
    await_ack(&state, "job-3", true).await;
    let terminal = await_terminal(&state, "job-3").await;
    assert_eq!(terminal.phase(), JobPhase::JobSucceeded);

    let out_raw = output_bytes(&state, "job-3");
    let out = String::from_utf8_lossy(&out_raw);
    let ws_str = ws_dir.to_string_lossy();
    // env 注入：MY_VAR=hello-cwd 出现在输出。
    assert!(out.contains("hello-cwd"), "env 应注入：{out}");
    // cwd = 工作区目录：探针文件落在工作区目录里（不经路径字符串比对，避开
    // Windows 8.3 短名 vs 长名渲染差异）。
    wait_until(|| async { probe.exists() }).await;
    assert!(
        probe.is_file(),
        "cwd 应为工作区目录（探针文件应落在 {}）：{out}",
        ws_dir.display()
    );
    // ${SISY_WORKSPACE} 执行前替换为工作区绝对路径（runner 展开即 ws_dir 字面量）。
    assert!(
        out.contains(ws_str.as_ref()),
        "${{SISY_WORKSPACE}} 应替换为工作区路径 {}：{out}",
        ws_dir.display()
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 机密注入 env + 输出字面量脱敏 `***` + 无匹配穿透。
#[tokio::test]
async fn redacts_secret_in_output_and_passthrough_unrelated() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 机密 SECRET=hunter2 经 env 下发、声明在 secrets；命令回显它。
    let cmd = if cfg!(unix) {
        "echo \"$SECRET\"; echo not-secret-text".to_string()
    } else {
        "echo $env:SECRET; echo not-secret-text".to_string()
    };
    send_downlink(
        &state,
        shell_spec(
            "job-4",
            "pipe",
            "job",
            &cmd,
            vec![("SECRET", "hunter2")],
            vec!["SECRET"],
            0,
        ),
    )
    .await;
    await_ack(&state, "job-4", true).await;
    await_terminal(&state, "job-4").await;

    let out_raw = output_bytes(&state, "job-4");
    let out = String::from_utf8_lossy(&out_raw);
    assert!(
        !out.contains("hunter2"),
        "机密值应被脱敏，不得出现在输出：{out}"
    );
    assert!(out.contains("***"), "机密替换为 ***：{out}");
    // 无匹配穿透：非机密文本原样。
    assert!(out.contains("not-secret-text"), "非机密文本不误脱敏：{out}");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 脱敏跨输出块边界（机密拆在两次 16KiB 读取之间）。
/// Unix only：用 /dev/zero + tr 造精确 16383 字节前缀，使 6 字节机密正好跨过
/// 16384 边界。Windows 无等价零依赖造数手段，跨块边界由 redact 单测覆盖。
#[cfg(unix)]
#[tokio::test]
async fn redacts_secret_across_chunk_boundary() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 16383 个 'x' + "CROSSME"（7 字节）= 16390 字节；首读 16384 = 16383 x + 'C'，
    // 机密 "CROSSME" 跨块边界（首块尾部 'C' + 次块 'ROSSME'）。
    let cmd = "( head -c 16383 /dev/zero | tr '\\0' x; printf CROSSME )";
    send_downlink(
        &state,
        shell_spec(
            "job-5",
            "pipe",
            "job",
            cmd,
            vec![("SECRET", "CROSSME")],
            vec!["SECRET"],
            0,
        ),
    )
    .await;
    await_ack(&state, "job-5", true).await;
    await_terminal(&state, "job-5").await;

    let out = output_bytes(&state, "job-5");
    assert!(
        !out.windows(7).any(|w| w == b"CROSSME"),
        "跨块机密应被脱敏：{:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        String::from_utf8_lossy(&out).contains("***"),
        "应插入 *** 标记"
    );
    // 前缀完整保留（16383 个 x）。
    let x_count = out.iter().filter(|&&b| b == b'x').count();
    assert_eq!(x_count, 16383, "非机密前缀应完整外发");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 超日志上限截断插 Truncated 标记、不判败。
#[tokio::test]
async fn truncates_log_over_limit_without_failing() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // log_limit=10 字节；命令输出远超（26 字母）。
    let cmd = if cfg!(unix) {
        "printf abcdefghijklmnopqrstuvwxyz"
    } else {
        // pwsh：Write-Host 不进 stdout，用 echo（Write-Output）。
        "echo abcdefghijklmnopqrstuvwxyz"
    };
    send_downlink(
        &state,
        shell_spec("job-6", "pipe", "job", cmd, vec![], vec![], 10),
    )
    .await;
    await_ack(&state, "job-6", true).await;
    let terminal = await_terminal(&state, "job-6").await;
    // 截断不判败：echo/printf 退出 0 → succeeded。
    assert_eq!(terminal.phase(), JobPhase::JobSucceeded, "截断不判败");
    // 出现 Truncated 标记。
    wait_until(|| async { has_truncated(&state, "job-6") }).await;
    assert!(has_truncated(&state, "job-6"), "超限应插 Truncated 标记");
    // 实际外发字节被截到上限（10）附近。
    let out = output_bytes(&state, "job-6");
    assert!(out.len() <= 10, "外发字节不超过上限 10：{} 字节", out.len());

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 取消 → 进程树终止、终态 cancelled。
#[tokio::test]
async fn cancel_terminates_and_reports_cancelled() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        shell_spec("job-7", "pipe", "job", &sleep_cmd(), vec![], vec![], 0),
    )
    .await;
    await_ack(&state, "job-7", true).await;
    wait_until(|| async {
        state
            .statuses()
            .iter()
            .any(|s| s.job_id == "job-7" && s.phase() == JobPhase::JobRunning)
    })
    .await;

    // 下发取消（build 级，job_id 同）。
    send_downlink(
        &state,
        ChannelMessage {
            kind: Some(Kind::Cancel(CancelBuild {
                build_id: "1".into(),
                job_id: "job-7".into(),
            })),
        },
    )
    .await;

    let started = std::time::Instant::now();
    let terminal = await_terminal(&state, "job-7").await;
    assert_eq!(terminal.phase(), JobPhase::JobCancelled, "取消 → cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "取消应 promptly 终止进程树（非睡满 30s）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 同 job 去重——已在跑再下发同 job_id → 拒收（accepted=false）。
#[tokio::test]
async fn dedup_rejects_already_running_job() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 首个：长睡眠（保持 running）。
    send_downlink(
        &state,
        shell_spec("job-8", "pipe", "job", &sleep_cmd(), vec![], vec![], 0),
    )
    .await;
    await_ack(&state, "job-8", true).await;
    wait_until(|| async {
        state
            .statuses()
            .iter()
            .any(|s| s.job_id == "job-8" && s.phase() == JobPhase::JobRunning)
    })
    .await;

    // 再次下发同 job_id → 拒收。
    send_downlink(
        &state,
        shell_spec("job-8", "pipe", "job", "echo hi", vec![], vec![], 0),
    )
    .await;
    let reject = await_ack(&state, "job-8", false).await;
    assert!(!reject.accepted, "已在跑应拒收");
    assert!(!reject.error.is_empty(), "拒收带原因：{}", reject.error);

    // 取消首个，释放。
    send_downlink(
        &state,
        ChannelMessage {
            kind: Some(Kind::Cancel(CancelBuild {
                build_id: "1".into(),
                job_id: "job-8".into(),
            })),
        },
    )
    .await;
    await_terminal(&state, "job-8").await;

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: stderr 流带 stderr 标记（stdout/stderr 合流、stream 标记正确）。
#[tokio::test]
async fn stderr_stream_tagged_correctly() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 写 stderr：Unix `echo err 1>&2`；pwsh `Write-Error` 进 stderr 但带前缀，
    // 用 `[Console]::Error.WriteLine` 直写 stderr。
    let cmd = if cfg!(unix) {
        "echo to-stdout; echo to-stderr 1>&2"
    } else {
        "[Console]::Error.WriteLine('to-stderr'); echo to-stdout"
    };
    send_downlink(
        &state,
        shell_spec("job-9", "pipe", "job", cmd, vec![], vec![], 0),
    )
    .await;
    await_ack(&state, "job-9", true).await;
    await_terminal(&state, "job-9").await;

    // 至少有一个 OutputChunk stream=stderr，且其 data 含 "to-stderr"。
    wait_until(|| async {
        state.log_batches().iter().any(|b| {
            b.job_id == "job-9"
                && b.events.iter().any(|ev| {
                    matches!(
                        (ev.kind.as_ref(), &ev),
                        (Some(EventKind::Output(o)), _) if o.stream == Stream::Stderr as i32
                            && String::from_utf8_lossy(&o.data).contains("to-stderr")
                    )
                })
        })
    })
    .await;
    // stdout 也有标记。
    assert!(
        state.log_batches().iter().any(|b| {
            b.job_id == "job-9"
                && b.events.iter().any(|ev| {
                    matches!(
                        ev.kind.as_ref(),
                        Some(EventKind::Output(o)) if o.stream == Stream::Stdout as i32
                    )
                })
        }),
        "stdout 应有 stdout stream 标记"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

// ============================================================
// checkout 步骤端到端（票 B3-T6 / #60）
// ============================================================

/// AC: JobSpec 含 checkout 步骤 → ack → 真实 clone 本地 git 仓库 → 终态 succeeded
/// + 工作区有仓库内容 + HEAD 钉到 commit。验证 dispatch → runner → checkout
/// → stepio → logbuf → channel 全链路（checkout 占位换入真实执行器）。
#[tokio::test]
async fn checkout_step_clones_local_repo_and_reports_success() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let (repo, sha) = local_git_repo(dir.path(), "src-repo");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        checkout_spec(
            "job-co",
            "pipe",
            "job",
            &repo.to_string_lossy(),
            "main",
            &sha,
            false,
        ),
    )
    .await;
    let ack = await_ack(&state, "job-co", true).await;
    assert!(ack.error.is_empty(), "接受不应带 error：{}", ack.error);
    let terminal = await_terminal(&state, "job-co").await;
    assert_eq!(terminal.phase(), JobPhase::JobSucceeded, "checkout 应成功");

    // 工作区有仓库内容 + HEAD 钉到 commit（经通道下发→runner→checkout 真实检出）。
    let ws_dir = ws.resolve("pipe", "job").expect("resolve 工作区");
    assert!(ws_dir.join("hello.txt").is_file(), "文件已检出");
    assert_eq!(ws_head(&ws_dir), sha, "HEAD 钉到 commit");
    // step 事件：checkout 步骤的 start + end（经 logbuf → LogBatch → fake）。
    wait_until(|| async { step_events(&state, "job-co").len() >= 2 }).await;
    let steps = step_events(&state, "job-co");
    assert_eq!(steps[0].exit_code, None, "start 事件 exit_code=None");
    assert!(
        steps[0].command.contains("git checkout"),
        "step start 命令回显为 checkout 摘要：{}",
        steps[0].command
    );
    assert_eq!(steps[1].exit_code, Some(0), "end 事件 exit_code=Some(0)");
    // 命令回显不含凭据（本例无凭据，亦不应含任何凭据占位）。
    assert!(!steps[0].command.contains("password"), "回显不含凭据");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: checkout 后续 shell 步骤在已检出工作区里跑——shell 读到 checkout 出的文件，
/// 证明步骤序贯 + cwd = 工作区 + checkout 真实落盘。
#[tokio::test]
async fn checkout_then_shell_step_runs_in_checked_out_workspace() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let (repo, sha) = local_git_repo(dir.path(), "src-repo");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // checkout（钉到 commit）+ shell 读 hello.txt 内容。Unix `cat`、Windows 默认
    // 解释器（pwsh/cmd）用 `type`。
    let shell_cmd = if cfg!(unix) {
        "cat hello.txt".to_string()
    } else {
        "type hello.txt".to_string()
    };
    send_downlink(
        &state,
        checkout_then_shell_spec(
            "job-cs",
            "pipe",
            "job",
            &repo.to_string_lossy(),
            "main",
            &sha,
            &shell_cmd,
        ),
    )
    .await;
    await_ack(&state, "job-cs", true).await;
    let terminal = await_terminal(&state, "job-cs").await;
    assert_eq!(
        terminal.phase(),
        JobPhase::JobSucceeded,
        "checkout + shell 应成功"
    );

    // shell 步骤在 checkout 出的工作区里读到了 hello.txt 内容（"v1"）。
    wait_until(|| async {
        output_bytes(&state, "job-cs")
            .windows(b"v1".len())
            .any(|w| w == b"v1")
    })
    .await;
    let out = output_bytes(&state, "job-cs");
    assert!(
        String::from_utf8_lossy(&out).contains("v1"),
        "shell 应读到 checkout 出的 hello.txt：{out:?}"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC6 集成层增量语义：首次 checkout（clone）后脏化工作区，再次 checkout（同
/// pipeline/job 名 → 复用工作区 → 增量 fetch + reset --hard + clean -fd）——
/// 还原跟踪文件、删未跟踪文件、HEAD 仍钉到 commit。AC「集成测试…验证增量语义」
/// 在集成层覆盖（非仅单元）。
#[tokio::test]
async fn checkout_step_incremental_resets_and_cleans_on_reused_workspace() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let (repo, sha) = local_git_repo(dir.path(), "src-repo");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));
    let ws_dir = ws.resolve("pipe", "job").expect("预解析工作区");

    // 首次 checkout（clone）。
    send_downlink(
        &state,
        checkout_spec(
            "job-inc1",
            "pipe",
            "job",
            &repo.to_string_lossy(),
            "main",
            &sha,
            false,
        ),
    )
    .await;
    await_ack(&state, "job-inc1", true).await;
    let t1 = await_terminal(&state, "job-inc1").await;
    assert_eq!(t1.phase(), JobPhase::JobSucceeded, "首次 clone 应成功");
    assert_eq!(ws_head(&ws_dir), sha, "HEAD 钉到 commit");
    assert!(ws_dir.join("hello.txt").is_file());

    // 脏化工作区：改跟踪文件 + 加未跟踪文件。
    std::fs::write(ws_dir.join("hello.txt"), "dirty\n").expect("改跟踪文件");
    std::fs::write(ws_dir.join("untracked.txt"), "junk\n").expect("加未跟踪文件");

    // 再次 checkout（同 pipeline/job → 复用工作区 → 增量 fetch+reset --hard+clean -fd）。
    send_downlink(
        &state,
        checkout_spec(
            "job-inc2",
            "pipe",
            "job",
            &repo.to_string_lossy(),
            "main",
            &sha,
            false,
        ),
    )
    .await;
    await_ack(&state, "job-inc2", true).await;
    let t2 = await_terminal(&state, "job-inc2").await;
    assert_eq!(t2.phase(), JobPhase::JobSucceeded, "增量 checkout 应成功");
    assert_eq!(ws_head(&ws_dir), sha, "增量后 HEAD 仍钉到 commit");
    assert_eq!(
        std::fs::read_to_string(ws_dir.join("hello.txt")).unwrap(),
        "v1\n",
        "reset --hard 还原跟踪文件"
    );
    assert!(
        !ws_dir.join("untracked.txt").exists(),
        "clean -fd 删未跟踪文件"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

// ============================================================
// 容器后端端到端（票 B3-T7 / #53；#[ignore]：需 docker daemon + 网络）
// ============================================================

/// 构造一个容器 shell 步骤的 JobSpec（`exec_env = Container{image}`）。
#[allow(clippy::too_many_arguments)]
fn container_shell_spec(
    job_id: &str,
    pipeline: &str,
    job: &str,
    image: &str,
    command: &str,
    env: Vec<(&str, &str)>,
    secrets: Vec<&str>,
    log_limit: i64,
) -> ChannelMessage {
    let env: HashMap<String, String> = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: log_limit,
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Shell(ShellStep {
                    command: command.into(),
                })),
            }],
            env,
            exec_env: Some(ExecutionEnv {
                kind: Some(EnvKind::Container(ContainerEnv {
                    image: image.into(),
                })),
            }),
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![],
            secrets: secrets.into_iter().map(str::to_string).collect(),
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// AC: 真实容器 shell 步骤执行 + 退出码 + 日志断言（`#[ignore]`：需 docker daemon
/// + 网络拉 `alpine:3.20`）。无 daemon 时本测试被忽略、装配/单测仍绿。
/// 覆盖：JobSpec(container) → ack → docker pull → docker run /bin/sh -c →
/// stdout 流式回传 → 终态 succeeded + 退出码 0；`${SISY_WORKSPACE}` 在容器内
/// 展开为 `/sisyphus/workspace`。
#[tokio::test]
#[ignore = "需 docker daemon + 网络（拉 alpine:3.20）；手动 `cargo test -- --ignored`"]
async fn container_shell_step_runs_in_docker_and_reports_success() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // echo hello + 回显 ${SISY_WORKSPACE}（runner 执行前展开为 /sisyphus/workspace）。
    send_downlink(
        &state,
        container_shell_spec(
            "job-c1",
            "pipe",
            "job",
            "alpine:3.20",
            r#"echo hello-container; echo "ws=${SISY_WORKSPACE}""#,
            vec![],
            vec![],
            0,
        ),
    )
    .await;
    let ack = await_ack(&state, "job-c1", true).await;
    assert!(ack.error.is_empty(), "接受不应带 error：{}", ack.error);

    let terminal = await_terminal(&state, "job-c1").await;
    assert_eq!(
        terminal.phase(),
        JobPhase::JobSucceeded,
        "容器 shell 步骤应成功：{}",
        terminal.detail
    );
    assert_eq!(terminal.exit_code, Some(0), "succeeded 退出码 0");

    // stdout 含 hello-container + ${SISY_WORKSPACE} 展开为 /sisyphus/workspace。
    wait_until(|| async {
        output_bytes(&state, "job-c1")
            .windows(b"hello-container".len())
            .any(|w| w == b"hello-container")
    })
    .await;
    let out = output_bytes(&state, "job-c1");
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.contains("hello-container"),
        "stdout 含 hello-container：{out}"
    );
    assert!(
        out.contains("/sisyphus/workspace"),
        "${{SISY_WORKSPACE}} 应在容器内展开为 /sisyphus/workspace：{out}"
    );

    // step 事件：start（命令回显）+ end（exit 0）。
    wait_until(|| async { step_events(&state, "job-c1").len() >= 2 }).await;
    let steps = step_events(&state, "job-c1");
    assert_eq!(steps[0].exit_code, None, "start 事件 exit_code=None");
    assert!(steps[0].command.contains("echo"), "start 携带命令回显");
    assert_eq!(steps[1].exit_code, Some(0), "end 事件 exit_code=Some(0)");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC: 容器 shell 步骤非零退出 → 终态 failed + 退出码（`#[ignore]`：需 daemon）。
/// `docker run` 透传容器内退出码。
#[tokio::test]
#[ignore = "需 docker daemon + 网络（拉 alpine:3.20）；手动 `cargo test -- --ignored`"]
async fn container_shell_step_failure_reports_failed_with_exit_code() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        container_shell_spec(
            "job-c2",
            "pipe",
            "job",
            "alpine:3.20",
            "exit 7",
            vec![],
            vec![],
            0,
        ),
    )
    .await;
    await_ack(&state, "job-c2", true).await;
    let terminal = await_terminal(&state, "job-c2").await;
    assert_eq!(terminal.phase(), JobPhase::JobFailed, "非零退出 → failed");
    assert_eq!(terminal.exit_code, Some(7), "携带容器内退出码 7");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 构造一个容器 checkout 步骤的 JobSpec（`exec_env = Container{image}`，git 钉到
/// 分支头）。
fn container_checkout_spec(
    job_id: &str,
    pipeline: &str,
    job: &str,
    image: &str,
    url: &str,
    branch: &str,
) -> ChannelMessage {
    ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: job_id.to_string(),
            pipeline_name: pipeline.into(),
            job_name: job.into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![JobStep {
                name: "step-0".into(),
                seq: 0,
                kind: Some(StepKind::Checkout(CheckoutStep {
                    vcs: VcsType::VcsGit as i32,
                    repo_url: url.into(),
                    r#ref: branch.into(),
                    commit: String::new(), // 分支头
                    submodules: false,
                })),
            }],
            env: HashMap::new(),
            exec_env: Some(ExecutionEnv {
                kind: Some(EnvKind::Container(ContainerEnv {
                    image: image.into(),
                })),
            }),
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![],
            secrets: vec![],
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    }
}

/// AC: 容器内 checkout（git 在镜像内、克隆到挂载工作区、钉到分支头）——`#[ignore]`：
/// 需 docker daemon + 网络（拉 `alpine/git` 镜像 + 克隆公网仓库）。覆盖 ADR-0018
/// 「checkout 在容器内执行不特例」端到端：pull → docker run git clone → 文件落盘
/// 到宿主工作区（经挂载）→ 终态 succeeded。
#[tokio::test]
#[ignore = "需 docker daemon + 网络（拉 alpine/git + 克隆公网仓库）；手动 `cargo test -- --ignored`"]
async fn container_checkout_step_clones_into_mounted_workspace() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // octocat/Hello-World：GitHub 长期稳定的小型公开仓库（含 README + 多分支）。
    // 分支头 checkout（commit 空 → Agent 检 origin/<branch>）。
    send_downlink(
        &state,
        container_checkout_spec(
            "job-co3",
            "pipe",
            "job",
            "alpine/git",
            "https://github.com/octocat/Hello-World.git",
            "master",
        ),
    )
    .await;
    await_ack(&state, "job-co3", true).await;
    let terminal = await_terminal(&state, "job-co3").await;
    assert_eq!(
        terminal.phase(),
        JobPhase::JobSucceeded,
        "容器内 git checkout 应成功：{}",
        terminal.detail
    );

    // 克隆结果经挂载落盘到宿主工作区（容器写 /sisyphus/workspace = 宿主 ws_dir）。
    let ws_dir = ws.resolve("pipe", "job").expect("resolve 工作区");
    assert!(
        ws_dir.join(".git").is_dir(),
        ".git 应经挂载落盘到宿主工作区（容器内克隆 → 挂载目录）"
    );
    assert!(
        ws_dir.join("README").is_file(),
        "README 应检出（Hello-World 仓库根含 README）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

// ============================================================
// 缓存 restore/save 时机与往返（ADR-0012 集成验收）
// ============================================================

/// AC（ADR-0012 时机 + 往返 + 跨任务复用）：job A 成功 → save 缓存；job B 同
/// pipeline 不同 job（独立工作区）restore → 命中（out 由 restore 拷入全新工作区）。
/// save 仅成功后；restore 内容与 save 内容逐字节一致（朴素拷贝往返）。
///
/// 命令刻意只用 `echo ... > out` / `echo ok`——sh / cmd / pwsh 三种默认解释器
/// 均认（`>` 两侧留空格 pwsh 才当重定向）；不在 shell 里判 HIT/MISS，改由测试
/// 直接读工作区文件断言 restore 结果。
#[tokio::test]
async fn cache_save_on_success_and_restore_across_jobs() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    let caches = vec![sisyphus_proto::agent::CacheSpec {
        key: "shared".into(),
        paths: vec!["out".into()],
        files: vec![],
    }];

    // job A：写 out → 成功 → save 缓存（paths=[out]）。job 名 "seed"。
    send_downlink(
        &state,
        shell_spec_with_caches("job-a", "pipe", "seed", "echo built > out", caches.clone()),
    )
    .await;
    let ack_a = await_ack(&state, "job-a", true).await;
    assert!(ack_a.error.is_empty());
    let term_a = await_terminal(&state, "job-a").await;
    assert_eq!(term_a.phase(), JobPhase::JobSucceeded, "job A 成功");
    // 缓存已 save：缓存目录存在 + out 非空（含 "built"）。
    let cache_dir = cache.root().join("pipe").join("shared");
    wait_until(|| async { cache_dir.join("out").is_file() }).await;
    let saved = std::fs::read(cache_dir.join("out")).unwrap();
    assert!(
        saved.windows(5).any(|w| w == b"built"),
        "save 拷入了 out（内容含 built）：{:?}",
        String::from_utf8_lossy(&saved)
    );

    // job B：同 pipeline "pipe"、不同 job "build" → 独立全新工作区（out 不存在）。
    // restore 在 shell 前 → 从缓存拷入 out；shell 仅 echo ok（不依赖 out）。
    send_downlink(
        &state,
        shell_spec_with_caches("job-b", "pipe", "build", "echo ok", caches.clone()),
    )
    .await;
    let _ = await_ack(&state, "job-b", true).await;
    let term_b = await_terminal(&state, "job-b").await;
    assert_eq!(term_b.phase(), JobPhase::JobSucceeded);
    // restore 命中：全新工作区的 out 由 restore 拷入，内容与 save 逐字节一致。
    let b_ws = ws.find("pipe", "build").expect("resolve job B 工作区");
    wait_until(|| async { b_ws.join("out").is_file() }).await;
    let restored = std::fs::read(b_ws.join("out")).unwrap();
    assert_eq!(
        restored, saved,
        "restore 拷入的内容与 save 一致（朴素拷贝往返）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC（ADR-0012 多 job 并行不踩踏）：两个 job 同 pipeline、不同 job（独立工作区）、
/// 同缓存 key 并发 save → 独占锁串行化、last-writer-wins、无损坏/无 panic、
/// registry 一致（单条）。并发经通道下发两帧、await 两者终态。
#[tokio::test]
async fn cache_parallel_jobs_same_key_no_stomp() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    let caches = vec![sisyphus_proto::agent::CacheSpec {
        key: "shared".into(),
        paths: vec!["out".into()],
        files: vec![],
    }];
    // 两 job 并发：各写不同内容到 out + save 同 key。独占锁串行 save。
    send_downlink(
        &state,
        shell_spec_with_caches("job-p1", "pipe", "p1", "echo v1 > out", caches.clone()),
    )
    .await;
    send_downlink(
        &state,
        shell_spec_with_caches("job-p2", "pipe", "p2", "echo v2 > out", caches.clone()),
    )
    .await;
    let _ = await_ack(&state, "job-p1", true).await;
    let _ = await_ack(&state, "job-p2", true).await;
    let t1 = await_terminal(&state, "job-p1").await;
    let t2 = await_terminal(&state, "job-p2").await;
    assert_eq!(t1.phase(), JobPhase::JobSucceeded, "job-p1 成功");
    assert_eq!(t2.phase(), JobPhase::JobSucceeded, "job-p2 成功");

    // last-writer-wins：缓存仅一条，out 内容为 v1 或 v2 之一（无损坏）。
    let cache_out = cache.root().join("pipe").join("shared").join("out");
    wait_until(|| async { cache_out.is_file() }).await;
    let content = std::fs::read(&cache_out).unwrap();
    assert!(
        content.windows(2).any(|w| w == b"v1") || content.windows(2).any(|w| w == b"v2"),
        "缓存内容为 v1 或 v2（last-writer-wins）：{:?}",
        String::from_utf8_lossy(&content)
    );
    // registry 一致：同 key 并发 save → 单条；记账大小 = out 文件大小。
    assert_eq!(
        cache.cache_bytes(),
        content.len() as i64,
        "registry 记账与落盘一致（单条，last-writer-wins）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC（ADR-0012 失败不 save）：shell 步骤非零退出 → 任务 failed → 不 save 缓存。
#[tokio::test]
async fn cache_not_saved_on_failure() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        shell_spec_with_caches(
            "job-f",
            "pipe",
            "job",
            &exit_cmd(3),
            vec![sisyphus_proto::agent::CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            }],
        ),
    )
    .await;
    let _ = await_ack(&state, "job-f", true).await;
    let term = await_terminal(&state, "job-f").await;
    assert_eq!(term.phase(), JobPhase::JobFailed, "shell 非零退出 → failed");
    // 失败不 save：缓存目录不存在、registry 空。
    let cache_dir = cache.root().join("pipe").join("k");
    assert!(!cache_dir.exists(), "失败不 save 缓存");
    assert_eq!(cache.cache_bytes(), 0, "registry 无条目");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC（ADR-0012「取消/超时/失败一律不 save」——取消分支）：长睡眠 job 带缓存声明
/// → running → 取消 → cancelled 终态 → 缓存不 save（目录不建、registry 空）。取消在
/// 步骤 wait 中电平触发、进程树终止、run_steps 早返回跳过 save_caches。
#[tokio::test]
async fn cache_not_saved_on_cancel() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    send_downlink(
        &state,
        shell_spec_with_caches(
            "job-cancel",
            "pipe",
            "job",
            &sleep_cmd(),
            vec![sisyphus_proto::agent::CacheSpec {
                key: "k".into(),
                paths: vec!["out".into()],
                files: vec![],
            }],
        ),
    )
    .await;
    await_ack(&state, "job-cancel", true).await;
    wait_until(|| async {
        state
            .statuses()
            .iter()
            .any(|s| s.job_id == "job-cancel" && s.phase() == JobPhase::JobRunning)
    })
    .await;

    // 下发取消（build 级，job_id 同）。
    send_downlink(
        &state,
        ChannelMessage {
            kind: Some(Kind::Cancel(CancelBuild {
                build_id: "1".into(),
                job_id: "job-cancel".into(),
            })),
        },
    )
    .await;
    let terminal = await_terminal(&state, "job-cancel").await;
    assert_eq!(terminal.phase(), JobPhase::JobCancelled, "取消 → cancelled");

    // 取消不 save：缓存目录不存在、registry 空。
    let cache_dir = cache.root().join("pipe").join("k");
    assert!(!cache_dir.exists(), "取消不 save 缓存");
    assert_eq!(cache.cache_bytes(), 0, "registry 无条目");

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC（ADR-0012 files 缺失 = fail-fast 点名）：caches 声明 files 但工作区无该
/// 文件 → 任务立即失败，detail 点名缺失文件。
#[tokio::test]
async fn cache_files_missing_fails_fast_naming_file() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, _cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 无 checkout 步骤 → restore 在首步骤前；声明 files=[Cargo.lock] 但工作区空
    // → fail-fast。
    send_downlink(
        &state,
        shell_spec_with_caches(
            "job-ff",
            "pipe",
            "job",
            "echo should-not-run",
            vec![sisyphus_proto::agent::CacheSpec {
                key: "rust".into(),
                paths: vec!["out".into()],
                files: vec!["Cargo.lock".into()],
            }],
        ),
    )
    .await;
    let _ = await_ack(&state, "job-ff", true).await;
    let term = await_terminal(&state, "job-ff").await;
    assert_eq!(term.phase(), JobPhase::JobFailed, "files 缺失 fail-fast");
    assert!(
        term.detail.contains("Cargo.lock"),
        "detail 点名缺失文件：{}",
        term.detail
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// AC（ADR-0012 restore 在最后一个 checkout 后、其余步骤前）：checkout 出
/// `Cargo.lock` → restore 据 lockfile 哈希命中（预先用同 lockfile 内容 seed 缓存）
/// → 后续 shell 步骤前 out 已由 restore 拷入。证明 restore 在 checkout 后、
/// files 哈希取自 checkout 后的 lockfile。
#[tokio::test]
async fn cache_restore_after_checkout_hits_using_lockfile_hash() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let (repo, sha) = local_git_repo_with_lockfile(dir.path(), "src-repo");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, ws, cache, agent_task) =
        spawn_agent_cache(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // 预先 seed 缓存：临时工作区放与仓库同内容 `Cargo.lock`（=b"lock-v1\n"，逐字
    // 节一致——files 哈希才相同）+ out=b"cached"，save 一条含 out 的缓存。
    let seed_ws = tempfile::tempdir().expect("seed 工作区");
    std::fs::write(seed_ws.path().join("Cargo.lock"), b"lock-v1\n").expect("写 lockfile");
    std::fs::write(seed_ws.path().join("out"), b"cached").expect("写 out");
    cache
        .save(
            "pipe",
            &sisyphus_proto::agent::CacheSpec {
                key: "rust".into(),
                paths: vec!["out".into()],
                files: vec!["Cargo.lock".into()],
            },
            seed_ws.path(),
        )
        .await;

    // 下发 [checkout, shell]：checkout 出 Cargo.lock（lock-v1\n）→ restore 据哈希
    // 命中（与 seed 同哈希）→ shell 前 out 已拷入。shell 仅 echo ok（不依赖 out）。
    let spec = ChannelMessage {
        kind: Some(Kind::JobSpec(Box::new(JobSpec {
            job_id: "job-co".to_string(),
            pipeline_name: "pipe".into(),
            job_name: "job".into(),
            build_number: 1,
            attempt: 0,
            log_limit_bytes: 0,
            steps: vec![
                JobStep {
                    name: "step-0".into(),
                    seq: 0,
                    kind: Some(StepKind::Checkout(CheckoutStep {
                        vcs: VcsType::VcsGit as i32,
                        repo_url: repo.to_string_lossy().to_string(),
                        r#ref: "main".into(),
                        commit: sha.clone(),
                        submodules: false,
                    })),
                },
                JobStep {
                    name: "step-1".into(),
                    seq: 1,
                    kind: Some(StepKind::Shell(ShellStep {
                        command: "echo ok".into(),
                    })),
                },
            ],
            env: HashMap::new(),
            exec_env: None,
            timeout_minutes: 0,
            uploads: vec![],
            downloads: vec![],
            caches: vec![sisyphus_proto::agent::CacheSpec {
                key: "rust".into(),
                paths: vec!["out".into()],
                files: vec!["Cargo.lock".into()],
            }],
            secrets: vec![],
            scm_credential: None,
            labels: vec![],
            retry_count: 0,
            allow_failure: false,
        }))),
    };
    send_downlink(&state, spec).await;
    let _ = await_ack(&state, "job-co", true).await;
    let term = await_terminal(&state, "job-co").await;
    assert_eq!(term.phase(), JobPhase::JobSucceeded, "checkout + restore + shell 成功");
    // restore 命中：工作区 out 由缓存拷入（=b"cached"，seed 的精确内容）。
    let co_ws = ws.find("pipe", "job").expect("resolve 工作区");
    wait_until(|| async { co_ws.join("out").is_file() }).await;
    assert_eq!(
        std::fs::read(co_ws.join("out")).unwrap(),
        b"cached",
        "restore 在 checkout 后据 lockfile 哈希命中"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

/// 创建本地 git 仓库并返回其绝对路径 + HEAD commit sha（含一个提交文件
/// `Cargo.lock` = b"lock-v1\n"——供 cache files 哈希集成测试）。
fn local_git_repo_with_lockfile(parent: &Path, name: &str) -> (PathBuf, String) {
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
    std::fs::write(repo.join("Cargo.lock"), "lock-v1\n").expect("写 lockfile");
    git(&["add", "Cargo.lock"]);
    git(&["commit", "--quiet", "-m", "v1"]);
    let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    (repo, sha)
}

// ============================================================
// 两后端切换（票 B3-T10 / #55）
// ============================================================

/// AC（B3-T10 / #55）：真实两后端切换——同一 agent 依次跑 host job 与 container
/// job，runner 按 `exec_env` 逐 job 选后端：host job（`exec_env=None`）在宿主机后端
/// 真实 shell 执行成功；container job（`exec_env=Container`）路由到容器后端、首步前
/// `docker pull` 失败 → `JobFailed`（detail 含 "docker pull"）。
///
/// 用永不可达的镜像 registry（`.invalid` TLD，RFC 2606 保留、DNS 必返 NXDOMAIN）
/// 保证无论宿主有无 docker / 网络都确定失败：无 docker 二进制 → spawn 失败；有
/// daemon → DNS/manifest 失败。不依赖 `#[ignore]` 门控，CI 确定性绿（覆盖「两后端
/// 切换」一条）；真实容器执行（pull 成功 + run）由既有 `#[ignore]` 用例覆盖。
#[tokio::test]
async fn backend_switches_per_job_host_runs_and_container_routes_to_pull() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let state = runner_state(Some("sisa_abc"));
    let (addr, server_task) = spawn_fake(state.clone()).await;
    let (shutdown_tx, _ws, agent_task) =
        spawn_agent(dir.path(), format!("http://{addr}"), Some("sisa_abc"));

    // host job：exec_env=None → 宿主机后端真实 shell 执行成功 + 输出 "host-ran"。
    send_downlink(
        &state,
        shell_spec("job-host", "pipe", "host", "echo host-ran", vec![], vec![], 0),
    )
    .await;
    let ack_h = await_ack(&state, "job-host", true).await;
    assert!(ack_h.error.is_empty(), "host job 接受：{}", ack_h.error);
    let term_h = await_terminal(&state, "job-host").await;
    assert_eq!(term_h.phase(), JobPhase::JobSucceeded, "host job 宿主机后端成功");
    wait_until(|| async {
        output_bytes(&state, "job-host")
            .windows(b"host-ran".len())
            .any(|w| w == b"host-ran")
    })
    .await;
    assert!(
        String::from_utf8_lossy(&output_bytes(&state, "job-host")).contains("host-ran"),
        "host job 输出 host-ran（宿主机后端真实执行）"
    );

    // container job：exec_env=Container{不可达 registry} → 容器后端、docker pull 失败。
    send_downlink(
        &state,
        container_shell_spec(
            "job-ctr",
            "pipe",
            "ctr",
            "no.such.registry.invalid/sisyphus:none",
            "echo should-not-run",
            vec![],
            vec![],
            0,
        ),
    )
    .await;
    let ack_c = await_ack(&state, "job-ctr", true).await;
    assert!(
        ack_c.accepted,
        "container job 接受（pull 在首步前、ack 时未知失败）"
    );
    let term_c = await_terminal(&state, "job-ctr").await;
    assert_eq!(term_c.phase(), JobPhase::JobFailed, "container job pull 失败 → failed");
    assert!(
        term_c.detail.contains("docker pull"),
        "container job 走容器后端（docker pull 失败，detail 点名）：{}",
        term_c.detail
    );
    // 容器命令未执行（pull 失败前置、未 docker run）。
    assert!(
        !String::from_utf8_lossy(&output_bytes(&state, "job-ctr")).contains("should-not-run"),
        "container job 命令未执行（pull 失败前置）"
    );

    shutdown_tx.send(true).expect("关闭");
    agent_task.await.expect("agent 退出");
    server_task.abort();
}

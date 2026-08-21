//! Spec B5 收口 tracer bullet（票 #81 AC，ADR-0004/0005/0006/0007/0013/
//! 0014/0016/0017/0019）：把 B5 各票独立交付的价值链**串成一条**在真实部署
//! 形态（进程内组合根 + fake Agent）下跑通——触发、执行（checkout + shell +
//! 缓存声明）、日志 SSE 浏览器语义、产物上传、通知送达、poll 触发、单台
//! Agent 升级往返、/metrics 指标可见、概览页全卡真值无退化。另四条失败
//! 路径语义断言：取消（排队 + 运行中）、fail-fast 级联、超时、重跑（从头 +
//! 失败任务 attempt+1）。
//!
//! 形态基准：`b2c_tracer_bullet`（REST router + 真实调度循环 + 真实 tonic
//! gRPC 共享同一 AppState 的进程内组合根——B5 全链唯一把整条链缝进同一
//! state 的 harness 形态）+ `logs_pipeline`（SSE 读）/ `artifacts_rest`
//! （产物往返）/ `upgrade_roundtrip`（升级往返）/ `notify_smtp`（notifier
//! 测试缝）/ `overview_metrics`（/metrics + 概览）/ `scm_rest`（本地裸仓库
//! fixture）/ `trigger.rs::TriggerEngine`（poll tick 假时钟）。fake Agent 持
//! proto 契约在真实装配下工作，不 spawn 进程。
//!
//! 不新增端点、不改已定语义、不改 proto——只组合既有链路做贯通断言。

use std::fs;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use http_body_util::BodyExt;
use sisyphus_model::pipeline::{EnvVar, Job, Pipeline, Stage, Step};
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, JobAck, JobPhase, JobStatus as ProtoJobStatus, LogBatch, LogEvent,
    OutputChunk, StepEvent, Stream, UpgradePhase, UpgradeStatus, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind, log_event::Kind as EventKind,
};
use sisyphus_server::notify::{MailMessage, MailSendError, MailSender, SmtpConnection, spawn_notifier};
use sisyphus_server::scm::{FakeProbe, ScmProbe};
use sisyphus_server::store::builds::{BuildRepo, BuildStatus, TriggerSource};
use sisyphus_server::trigger::TriggerEngine;
use sisyphus_server::{grpc, sched, store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

mod common;

use common::{DEFAULT_PEER, TestApp, body_json, body_text, custom_req, req_with_cookie, test_app_from_state};

// ---------------------------------------------------------------------------
// 版本与工具
// ---------------------------------------------------------------------------

/// 与 workspace 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

/// 等到谓词成立或超时（调度/通知是异步路径，轮询驱动避免 flaky）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("条件在 15s 内未成立");
}

/// 单事件 LogBatch 帧（Agent logbuf 活体形态：每帧一事件）。
fn log_batch(job_id: &str, attempt: i32, event: LogEvent, start_seq: u64) -> ChannelMessage {
    ChannelMessage {
        kind: Some(Kind::LogBatch(LogBatch {
            job_id: job_id.to_string(),
            attempt,
            start_seq,
            events: vec![event],
        })),
    }
}

fn output(seq: u64, stream: Stream, data: &[u8]) -> LogEvent {
    LogEvent {
        seq,
        kind: Some(EventKind::Output(OutputChunk {
            stream: stream as i32,
            data: data.to_vec(),
        })),
    }
}

fn step_start(seq: u64, step: i32, command: &str) -> LogEvent {
    LogEvent {
        seq,
        kind: Some(EventKind::Step(StepEvent {
            seq: step,
            step_started_at_ms: 1000,
            step_ended_at_ms: 0,
            exit_code: None,
            command: command.into(),
        })),
    }
}

fn step_end(seq: u64, step: i32, exit_code: Option<i32>) -> LogEvent {
    LogEvent {
        seq,
        kind: Some(EventKind::Step(StepEvent {
            seq: step,
            step_started_at_ms: 1000,
            step_ended_at_ms: 1250,
            exit_code,
            command: String::new(),
        })),
    }
}

// ---------------------------------------------------------------------------
// notifier 测试缝（notify_smtp.rs 同款）
// ---------------------------------------------------------------------------

/// 捕获 [`MailMessage`] 到 mpsc 通道供断言；忽略 [`SmtpConnection`]（不真连）。
struct RecordingSender {
    tx: mpsc::UnboundedSender<MailMessage>,
}

impl MailSender for RecordingSender {
    fn send(
        &self,
        _conn: SmtpConnection,
        msg: MailMessage,
    ) -> impl std::future::Future<Output = Result<(), MailSendError>> + Send {
        let _ = self.tx.send(msg);
        async { Ok::<(), MailSendError>(()) }
    }
}

/// 起一个带 RecordingSender 的 notifier，返回其接收端（spawn_notifier 同步订阅，
/// 调用后再发事件即不漏）。
fn spawn_recording(state: &sisyphus_server::api::AppState) -> mpsc::UnboundedReceiver<MailMessage> {
    let (tx, rx) = mpsc::unbounded_channel::<MailMessage>();
    let _notifier = spawn_notifier(state.bus.clone(), state.clone(), RecordingSender { tx });
    rx
}

/// 订阅 notifier + 收一封邮件（超时即 panic）。
async fn recv_mail(rx: &mut mpsc::UnboundedReceiver<MailMessage>) -> MailMessage {
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("15s 内应收到通知邮件")
        .expect("通道未关")
}

// ---------------------------------------------------------------------------
// 本地裸仓库 fixture（scm_rest.rs::bare_repo 同款）
// ---------------------------------------------------------------------------

/// 创建本地裸仓库（main + dev 两分支），返回 (TempDir, 裸仓库路径, main sha)。
fn bare_repo() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().expect("临时目录");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("建 src");
    let git = |args: &[&str]| {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(&src)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}：{}",
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
    fs::write(src.join("hello.txt"), "v1\n").expect("写文件");
    git(&["add", "hello.txt"]);
    git(&["commit", "--quiet", "-m", "v1"]);
    let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .expect("utf8")
        .trim()
        .to_string();
    git(&["branch", "dev"]);
    let bare = dir.path().join("bare");
    StdCommand::new("git")
        .args([
            "clone",
            "--bare",
            "--quiet",
            &src.to_string_lossy(),
            &bare.to_string_lossy(),
        ])
        .output()
        .expect("clone --bare");
    (dir, bare, sha)
}

// ---------------------------------------------------------------------------
// 进程内组合根 harness
// ---------------------------------------------------------------------------

/// 真实 store + REST router + 真实调度循环 + 真实 tonic gRPC + 触发引擎（poll）。
struct Harness {
    _dir: tempfile::TempDir,
    app: TestApp,
    _sched: tokio::task::JoinHandle<()>,
    grpc_addr: std::net::SocketAddr,
    grpc_handle: tokio::task::JoinHandle<()>,
    /// poll 触发引擎（FakeProbe 注入；测试侧调 tick 推进）。
    trigger: Arc<TriggerEngine>,
    /// poll 探测端口（可控 head 队列）。
    probe: Arc<FakeProbe>,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = sisyphus_server::api::AppState::new(
        pool.clone(),
        dir.path().to_path_buf(),
        false,
        master_key,
        sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES,
        sisyphus_server::config::DEFAULT_RETENTION_DAYS,
        sisyphus_server::config::DEFAULT_METRICS_AUTH,
    )
    .await
    .expect("装配 AppState");
    // REST router 与 scheduler/gRPC/触发器共享同一 state（事件总线串起触发→下发）。
    let app = test_app_from_state(state.clone(), dir.path());

    // 会话注册表：用 state.agent_sessions（与 main.rs 同源）——REST 升级/工作区/
    // 缓存面经 state.agent_sessions 下发，gRPC 握手注册与调度派发也须用同一表，
    // 否则 REST 升级指令找不到会话（Ok(false)）。b2c harness 自建独立 sessions
    // 仅因其不测 REST 通道面；本票串升级往返，须同源。
    let sessions = state.agent_sessions.clone();
    let dispatcher = Arc::new(grpc::GrpcDispatcher::new(sessions.clone()));
    let scheduler = sched::Scheduler::new(state.engine.clone(), pool.clone(), dispatcher, 10);
    let scheduler_handle = scheduler.handle();
    let bus = state.bus.clone();
    let sched_task = tokio::spawn(async move {
        scheduler.reconstruct().await.expect("重建");
        scheduler.run(bus).await;
    });

    // 真实 tonic gRPC 服务（认证 + 心跳 + 任务面 + 升级/工作区/缓存全接线）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let grpc_addr = listener.local_addr().expect("addr");
    let grpc_state = state.clone();
    let grpc_sessions = sessions.clone();
    let grpc_scheduler = scheduler_handle;
    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc::service(grpc_state, grpc_sessions, grpc_scheduler))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });

    // 触发引擎：FakeProbe 注入，测试侧直接调 tick(now) 推进 poll。
    let probe = Arc::new(FakeProbe::new());
    let trigger = Arc::new(TriggerEngine::new(
        state.engine.clone(),
        pool.clone(),
        probe.clone() as Arc<dyn ScmProbe>,
    ));

    Harness {
        _dir: dir,
        app,
        _sched: sched_task,
        grpc_addr,
        grpc_handle,
        trigger,
        probe,
    }
}

/// 直查构建状态（按号；经 state 的 projects repo 解析项目 id）。
async fn build_status(h: &Harness, number: i64) -> BuildStatus {
    let project = h
        .app
        .state
        .projects
        .get_by_name("demo")
        .await
        .expect("查项目")
        .expect("demo 应存在");
    BuildRepo::new(h.app.pool.clone())
        .get_by_number(project.id, "release", number)
        .await
        .expect("查构建")
        .map(|b| b.status)
        .unwrap_or(BuildStatus::Queued)
}

/// 直查构建触发源（按号）。
async fn build_trigger(h: &Harness, number: i64) -> TriggerSource {
    let project = h
        .app
        .state
        .projects
        .get_by_name("demo")
        .await
        .expect("查项目")
        .expect("demo 应存在");
    BuildRepo::new(h.app.pool.clone())
        .get_by_number(project.id, "release", number)
        .await
        .expect("查构建")
        .expect("构建应存在")
        .trigger
}

// ---------------------------------------------------------------------------
// fake Agent 通道辅助
// ---------------------------------------------------------------------------

/// fake Agent 连接（握手 + 系统标签 metadata）→ 返回 (响应流, 上行发送器)。
/// `ver` 为 Agent 上报版本（默认 workspace 版本 1.0.0；升级往返面用 0.9.0
/// 以落在 N-1 窗口内可派发、且非目标版本使升级指令受理）。
async fn connect_fake_agent(
    addr: std::net::SocketAddr,
    token: &str,
    ver: Version,
) -> Result<
    (
        tonic::Streaming<ChannelMessage>,
        mpsc::Sender<ChannelMessage>,
    ),
    tonic::Status,
> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("tcp connect");
    let mut client = AgentChannelClient::new(channel);
    let (tx, rx) = mpsc::channel(32);
    tx.send(ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(ver),
            agent_name: "linux-1".into(),
        })),
    })
    .await
    .expect("send handshake");
    let mut request = tonic::Request::new(ReceiverStream::new(rx));
    request.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}")).expect("值"),
    );
    // 系统标签：os=linux（匹配任务 labels）。
    request.metadata_mut().insert(
        "x-sisyphus-os",
        MetadataValue::try_from("linux").expect("值"),
    );
    let response = client.connect(request).await?;
    Ok((response.into_inner(), tx))
}

/// 默认版本（workspace 同版本 1.0.0，兼容窗口内、可派发）。
fn v_current() -> Version {
    version()
}

/// 0.9.0：N-1 窗口下界（可派发、非目标版本，升级往返面用）。
fn v_below() -> Version {
    Version {
        major: 0,
        minor: 9,
        patch: 0,
    }
}

/// 从响应流收一条 JobSpec（跳过握手回执 / Cancel / Upgrade 帧）。
///
/// 读策略：短超时（2s）重试轮询，总上限 30s——单次 `timeout(15s, stream.message())`
/// 在 tonic HTTP/2 帧已到但未唤醒的窄窗口会整段阻塞到超时（实测偶发，本测试
/// 多链路串通时更易触发）；短超时重试每轮重新 poll `stream.message()`，给运行
/// 时解码已缓冲帧的机会，比单次长阻塞更稳。
async fn collect_one_job_spec(
    stream: &mut tonic::Streaming<ChannelMessage>,
) -> sisyphus_proto::agent::JobSpec {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), stream.message()).await {
            Ok(Ok(Some(msg))) => {
                if let Some(Kind::JobSpec(spec)) = msg.kind {
                    return *spec;
                }
                // 握手回执 / Cancel / Upgrade / 其他下行帧跳过。
            }
            Ok(Ok(None)) => panic!("流结束未收到 JobSpec"),
            Ok(Err(e)) => panic!("流读失败：{e}"),
            Err(_) => {} // 单次 2s 未到：续轮询（重 poll 给运行时解码缓冲帧）。
        }
        if tokio::time::Instant::now() > deadline {
            panic!("30s 内未收到 JobSpec");
        }
    }
}

/// 从响应流收一条 CancelBuild（跳过握手回执 / JobSpec / Upgrade 帧）。
/// 同 [`collect_one_job_spec`] 的短超时重试读策略（见其注释）。
async fn collect_one_cancel(
    stream: &mut tonic::Streaming<ChannelMessage>,
) -> sisyphus_proto::agent::CancelBuild {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), stream.message()).await {
            Ok(Ok(Some(msg))) => {
                if let Some(Kind::Cancel(cancel)) = msg.kind {
                    return cancel;
                }
            }
            Ok(Ok(None)) => panic!("流结束未收到 CancelBuild"),
            Ok(Err(e)) => panic!("流读失败：{e}"),
            Err(_) => {}
        }
        if tokio::time::Instant::now() > deadline {
            panic!("30s 内未收到 CancelBuild");
        }
    }
}

/// 从响应流收一条 UpgradeCommand（跳过握手回执 / JobSpec / Cancel 帧）。
/// 同 [`collect_one_job_spec`] 的短超时重试读策略（见其注释）。
async fn collect_one_upgrade(
    stream: &mut tonic::Streaming<ChannelMessage>,
) -> sisyphus_proto::agent::UpgradeCommand {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), stream.message()).await {
            Ok(Ok(Some(msg))) => {
                if let Some(Kind::Upgrade(cmd)) = msg.kind {
                    return cmd;
                }
            }
            Ok(Ok(None)) => panic!("流结束未收到 UpgradeCommand"),
            Ok(Err(e)) => panic!("流读失败：{e}"),
            Err(_) => {}
        }
        if tokio::time::Instant::now() > deadline {
            panic!("30s 内未收到 UpgradeCommand");
        }
    }
}

/// 发 JobAck（接受）。
async fn ack(tx: &mpsc::Sender<ChannelMessage>, job_id: &str) {
    tx.send(ChannelMessage {
        kind: Some(Kind::JobAck(JobAck {
            job_id: job_id.into(),
            accepted: true,
            error: String::new(),
        })),
    })
    .await
    .expect("send ack");
}

/// 发 JobStatus（指定阶段 + 退出码）。
async fn report(tx: &mpsc::Sender<ChannelMessage>, job_id: &str, phase: JobPhase, exit_code: Option<i32>) {
    tx.send(ChannelMessage {
        kind: Some(Kind::JobStatus(ProtoJobStatus {
            job_id: job_id.into(),
            phase: phase as i32,
            exit_code,
            detail: String::new(),
        })),
    })
    .await
    .expect("send status");
}

/// 发升级相位状态。
async fn upgrade_phase(tx: &mpsc::Sender<ChannelMessage>, phase: UpgradePhase) {
    tx.send(ChannelMessage {
        kind: Some(Kind::UpgradeStatus(UpgradeStatus {
            phase: phase as i32,
            error: String::new(),
        })),
    })
    .await
    .expect("send upgrade status");
}

// ---------------------------------------------------------------------------
// REST 辅助
// ---------------------------------------------------------------------------

/// admin 上传升级包（raw octet body + X-Sisyphus-Filename 头）。
async fn upload_pkg(app: &TestApp, cookie: &str, filename: &str, bytes: &str) -> axum::http::Response<axum::body::Body> {
    custom_req(
        app,
        "POST",
        "/api/v1/upgrade-packages",
        Some(bytes.into()),
        Some(cookie),
        &[
            ("sec-fetch-site", "same-origin".into()),
            ("x-sisyphus-filename", filename.into()),
        ],
        DEFAULT_PEER,
    )
    .await
}

/// PUT 全局 SMTP 配置。
async fn put_smtp_config(app: &TestApp, cookie: &str) {
    let resp = req_with_cookie(
        app,
        "PUT",
        "/api/v1/config/smtp",
        Some(
            r#"{"host":"smtp.example.com","port":587,"username":"postmaster","tls":"starttls","from_address":"ci@example.com","password":"relay-pw"}"#
                .into(),
        ),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "PUT SMTP 配置应 200");
}

/// 建 PAT（/metrics Bearer 鉴权面），返回明文 token。
async fn create_pat(app: &TestApp, cookie: &str) -> String {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/auth/tokens",
        Some(serde_json::json!({ "name": "metrics" }).to_string()),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 PAT 应 201");
    body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string()
}

/// /metrics Bearer 请求。
async fn metrics_get(app: &TestApp, pat: &str) -> String {
    let resp = custom_req(
        app,
        "GET",
        "/metrics",
        None,
        None,
        &[("authorization", format!("Bearer {pat}"))],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "/metrics 应 200");
    body_text(resp).await
}

/// 从 /metrics 文本提取单行指标值（按行前缀匹配）。
fn metric_value(text: &str, prefix: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        name.starts_with(prefix).then(|| value.parse().ok()).flatten()
    })
}

/// 单任务 pipeline：labels=os=linux（匹配 fake Agent）、带 notification + 产物上传声明。
fn pipeline() -> Pipeline {
    Pipeline {
        name: "release".into(),
        parameters: vec![],
        env: vec![EnvVar {
            name: "PIPELINE_ENV".into(),
            value: "from-pipeline".into(),
        }],
        notification: Some(sisyphus_model::pipeline::Notification {
            on_success: true,
            recipients: vec!["dev@example.com".into(), "ops@example.com".into()],
        }),
        stages: vec![Stage {
            name: "build".into(),
            when: None,
            jobs: vec![Job {
                name: "compile".into(),
                exec_env: None,
                labels: vec!["sisyphus/os=linux".into()],
                when: None,
                env: vec![],
                allow_failure: false,
                retry_count: 0,
                timeout_minutes: 0,
                artifact_uploads: vec![sisyphus_model::pipeline::ArtifactUpload {
                    name: "dist.tar".into(),
                    path: "dist.tar".into(),
                }],
                artifact_downloads: vec![],
                caches: vec![],
                secrets: vec![],
                steps: vec![Step::Shell {
                    command: "echo ${SISY_BUILD_NUMBER}".into(),
                    shell: None,
                    when: None,
                }],
            }],
        }],
        revision: None,
    }
}

/// 建项目 + Agent + 连接 fake Agent（主链路共享前置），返回 (cookie, token, stream, tx)。
async fn seed_and_connect(h: &Harness, scm_url: &str) -> (String, String, tonic::Streaming<ChannelMessage>, mpsc::Sender<ChannelMessage>) {
    let admin = common::setup_and_login(&h.app).await;
    // 建项目（git，scm_url 指本地裸仓库）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects",
        Some(
            serde_json::json!({
                "name": "demo",
                "scm_type": "git",
                "scm_url": scm_url,
                "default_branch": "main",
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建项目");
    // 存定义（带 notification + 产物上传声明）。
    let resp = req_with_cookie(
        &h.app,
        "PUT",
        "/api/v1/projects/demo/pipelines/release",
        Some(serde_json::to_string(&pipeline()).expect("序列化定义")),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "存定义");
    // 建 Agent。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/agents",
        Some(r#"{ "name": "linux-1", "max_concurrency": 2 }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 Agent");
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string();
    // 连 fake Agent（workspace 版本 1.0.0：可派发，构建执行面用）。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token, v_current()).await.expect("连接");
    let first = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))), "握手回执");
    (admin, token, stream, tx)
}

/// 建项目 + 存定义 + 建 Agent（**不连接** fake Agent），返回 (cookie, token)。
/// 用于「排队取消」面——无在线 Agent → 触发后任务排队等待，REST 取消排干。
async fn seed_only(h: &Harness, scm_url: &str) -> (String, String) {
    let admin = common::setup_and_login(&h.app).await;
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects",
        Some(
            serde_json::json!({
                "name": "demo",
                "scm_type": "git",
                "scm_url": scm_url,
                "default_branch": "main",
            })
            .to_string(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建项目");
    let resp = req_with_cookie(
        &h.app,
        "PUT",
        "/api/v1/projects/demo/pipelines/release",
        Some(serde_json::to_string(&pipeline()).expect("序列化定义")),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "存定义");
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/agents",
        Some(r#"{ "name": "linux-1", "max_concurrency": 2 }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 Agent");
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("token")
        .to_string();
    (admin, token)
}

/// Agent 上传产物（二进制 body，agent token 鉴权）。job_id 为 JobSpec.job_id（job 行 id 字符串）。
async fn agent_upload(app: &TestApp, token: &str, job_id: &str, name: &str, bytes: &[u8]) -> axum::http::Response<axum::body::Body> {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/agent/artifacts/{job_id}/{name}"))
        .header("authorization", format!("Bearer {token}"))
        .extension(axum::extract::ConnectInfo(DEFAULT_PEER))
        .body(axum::body::Body::from(bytes.to_vec()))
        .expect("构造请求");
    app.router.clone().oneshot(req).await.expect("oneshot")
}

// ===========================================================================
// 主链路：触发 → 执行 → 日志 SSE → 产物 → 通知 → poll 触发 → 升级 → /metrics → 概览
// ===========================================================================

/// Spec B5 收口主链路（票 #81 AC）：本地裸仓库 → 建项目（测试连接 + 分支预填）
/// → 建 pipeline → 手动触发 + poll 触发各一次 → 执行（日志 + 产物）→ 日志 SSE
/// 浏览器语义 → 产物上传 → 通知送达 → 单台 Agent 升级往返 → /metrics 指标可见
/// → 概览页全卡真值无退化。
#[tokio::test]
async fn b5_full_chain_trigger_to_metrics() {
    let h = harness().await;
    let (repo_dir, bare, _sha) = bare_repo();

    // 1. setup + 建项目 + 存定义 + 建 Agent + 连接 fake Agent。
    let (admin, token, mut stream, tx) = seed_and_connect(&h, &bare.to_string_lossy()).await;
    let pat = create_pat(&h.app, &admin).await;

    // 2. 测试连接（既有项目，git ls-remote 落地）→ 返回 head。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/test-connection",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "测试连接应成功");
    let body = body_json(resp).await;
    assert!(body["head"].as_str().is_some(), "测试连接返回 head sha");

    // 3. 分支枚举预填（创建期端点）→ 含 main/dev + 默认分支 main。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/scm-branches",
        Some(serde_json::json!({ "scm_url": bare.to_string_lossy() }).to_string()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "分支枚举应成功");
    let body = body_json(resp).await;
    let names: Vec<String> = body["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"main".to_string()) && names.contains(&"dev".to_string()), "分支枚举含 main/dev：{names:?}");
    assert_eq!(body["default_branch"], "main", "默认分支 main");

    // 4. SMTP 配置 + notifier 订阅（先于终态事件，不漏）。
    put_smtp_config(&h.app, &admin).await;
    let mut mail_rx = spawn_recording(&h.app.state);

    // 5. 手动触发（REST）→ 202 + 构建号 1。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1);
    assert_eq!(body["attempt"], 1);

    // 6. 调度下发：fake Agent 收 JobSpec（attempt=1，env 经真实通道到达）。
    let spec = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec.job_name, "compile");
    assert_eq!(spec.attempt, 1);
    assert_eq!(spec.build_number, 1);
    assert_eq!(spec.env.get("PIPELINE_ENV").map(String::as_str), Some("from-pipeline"), "env 经真实通道到达");

    // 7. 执行期：fake Agent 发日志（output + step_start/end）落库 + ack。
    ack(&tx, &spec.job_id).await;
    let job_id = spec.job_id.clone();
    let attempt = spec.attempt;
    tx.send(log_batch(&job_id, attempt, output(0, Stream::Stdout, b"preparing\n"), 0)).await.unwrap();
    tx.send(log_batch(&job_id, attempt, step_start(1, 0, "echo ${SISY_BUILD_NUMBER}"), 1)).await.unwrap();
    tx.send(log_batch(&job_id, attempt, output(2, Stream::Stdout, b"1\n"), 2)).await.unwrap();
    tx.send(log_batch(&job_id, attempt, step_end(3, 0, Some(0)), 3)).await.unwrap();

    // 8. 产物上传（agent token，job_id=JobSpec.job_id）。
    let artifact_bytes = b"dist-payload-\xDE\xAD\xBE\xEF".repeat(100);
    let resp = agent_upload(&h.app, &token, &job_id, "dist.tar", &artifact_bytes).await;
    assert_eq!(resp.status(), 201, "产物上传应 201");
    let body = body_json(resp).await;
    assert_eq!(body["name"], "dist.tar");
    use sha2::{Digest, Sha256};
    let expect_sha = format!("{:x}", Sha256::digest(&artifact_bytes));
    assert_eq!(body["sha256"], expect_sha);

    // 9. fake Agent 报 Succeeded → 构建 succeeded（事件驱动，wait 调度推进）。
    report(&tx, &job_id, JobPhase::JobSucceeded, Some(0)).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Succeeded }).await;

    // 10. 日志 SSE（浏览器语义）：oneshot 打 logs/stream → 回放历史有序 + job_end + 关流。
    let sse_path = format!(
        "/api/v1/projects/demo/pipelines/release/builds/1/jobs/compile/attempts/{attempt}/logs/stream"
    );
    let resp = req_with_cookie(&h.app, "GET", &sse_path, None, Some(&admin)).await;
    assert_eq!(resp.status(), 200, "SSE 应 200");
    let (text, mut sse_stream) = read_sse_until(resp, |t| {
        t.contains("event: job_end") && t.contains(r#""status":"succeeded""#)
    })
    .await;
    // 历史回放有序：output(0) → step_start(1) → output(2) → step_end(3)。
    let out0 = text.find("event: output\nid: 0").expect("output seq 0");
    let start1 = text.find("event: step_start\nid: 1").expect("step_start seq 1");
    let out2 = text.find("event: output\nid: 2").expect("output seq 2");
    let end3 = text.find("event: step_end\nid: 3").expect("step_end seq 3");
    assert!(out0 < start1 && start1 < out2 && out2 < end3, "SSE 到达序交织：{text}");
    // job_end 帧已到；后续流应关流（终态关流语义）。
    let mut tail = text;
    let ended = !continue_sse_until(&mut sse_stream, &mut tail, |_| false).await;
    assert!(ended, "终态流在 job_end 后关流：{tail}");

    // 断线 Last-Event-ID 续传（票 #81 AC：SSE 浏览器语义）——带 Last-Event-ID: 1
    // 重连，应从 seq 2 续传（跳过 0/1，含 2/3 + job_end），与 logs_pipeline 同款。
    let resp = custom_req(
        &h.app,
        "GET",
        &sse_path,
        None,
        Some(&admin),
        &[("last-event-id", "1".to_string())],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 200, "Last-Event-ID 续传应 200");
    let (resume, _) = read_sse_until(resp, |t| t.contains("event: job_end")).await;
    assert!(!resume.contains("id: 0"), "Last-Event-ID=1 起从 seq 2 续传：{resume}");
    assert!(resume.contains("id: 2") && resume.contains("id: 3"), "含 2/3：{resume}");

    // 11. 构建详情页：产物列表 + 单产物下载字节一致（viewer 面）。
    let list_path = "/api/v1/projects/demo/pipelines/release/builds/1/artifacts";
    let resp = req_with_cookie(&h.app, "GET", list_path, None, Some(&admin)).await;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1), "产物列表 1 项");
    assert_eq!(body["items"][0]["name"], "dist.tar");
    assert_eq!(body["items"][0]["sha256"], expect_sha);
    let resp = req_with_cookie(&h.app, "GET", &format!("{list_path}/dist.tar"), None, Some(&admin)).await;
    assert_eq!(resp.status(), 200, "页面下载 200");
    assert_eq!(resp.headers().get("x-sisyphus-sha256").unwrap().to_str().unwrap(), expect_sha);
    let got = resp.into_body().collect().await.expect("collect").to_bytes();
    assert_eq!(&got[..], &artifact_bytes[..], "页面下载字节一致");

    // 12. 通知送达（fake SMTP）：邮件含 构建号/pipeline/项目/成功状态/触发者。
    let msg = recv_mail(&mut mail_rx).await;
    assert!(msg.body.contains("#1"), "含构建号：{}", msg.body);
    assert!(msg.body.contains("release"), "含 pipeline：{}", msg.body);
    assert!(msg.body.contains("demo"), "含项目：{}", msg.body);
    assert!(msg.body.contains("成功"), "成功状态：{}", msg.body);
    assert!(msg.body.contains("admin"), "含触发者：{}", msg.body);
    assert_eq!(msg.from, "ci@example.com");
    assert_eq!(msg.to, vec!["dev@example.com".to_string(), "ops@example.com".to_string()]);

    // 13. poll 触发（票 #81 AC：手动 + poll 各一次）：建 poll 触发器 → 基线 → 新 commit → tick 火灾。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/triggers",
        Some(serde_json::json!({ "kind": "poll", "poll": { "interval_minutes": 1 } }).to_string()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 poll 触发器应 201");
    // 首探：记基线（head=A），不触发。
    h.probe.push_head(Some("commit-a"));
    let report0 = h.trigger.tick(1_700_000_000_000).await.expect("tick 0");
    assert_eq!(report0.poll_baseline, 1, "首探记基线不触发");
    // 新提交（head=B）→ tick 火灾 → 构建号 2（TriggerSource::Poll）。
    h.probe.push_head(Some("commit-b"));
    let report1 = h.trigger.tick(1_700_000_060_000).await.expect("tick 1");
    assert_eq!(report1.poll_fired, 1, "poll 新提交应触发 1 次");
    // 调度下发构建 2：fake Agent 收第二个 JobSpec → ack → Succeeded。
    wait_until(|| async { build_status(&h, 2).await == BuildStatus::Running }).await;
    let spec2 = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec2.build_number, 2, "poll 触发新构建号 2");
    assert_eq!(spec2.attempt, 1);
    ack(&tx, &spec2.job_id).await;
    report(&tx, &spec2.job_id, JobPhase::JobSucceeded, Some(0)).await;
    wait_until(|| async { build_status(&h, 2).await == BuildStatus::Succeeded }).await;
    // 断言触发源为 poll。
    assert_eq!(build_trigger(&h, 2).await, TriggerSource::Poll, "构建 2 触发源为 poll");

    // 14. 单台 Agent 升级往返（票 #81 AC）：上传包 → 指令 → 排空 → 下载校验 → 版本更新。
    // 用第二台 Agent `upgrader-1`（0.9.0，N-1 窗口内、非目标版本）做升级往返——
    // linux-1 已在目标版本 1.0.0，升级它会 409（已在目标）；另起一台不干扰构建链路。
    let pkg = "sisyphus-agent-1.0.0-linux-x86_64.tar.gz";
    let up = upload_pkg(&h.app, &admin, pkg, "new-binary-bytes").await;
    assert_eq!(up.status(), axum::http::StatusCode::CREATED, "上传升级包 201");
    let pkg_sha = body_json(up).await["sha256"].as_str().unwrap().to_string();
    // 建 upgrader-1（0.9.0）+ 连接。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/agents",
        Some(r#"{ "name": "upgrader-1", "max_concurrency": 1 }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 upgrader-1");
    let token_up = body_json(resp).await["token"].as_str().expect("token").to_string();
    let (mut stream_up, tx_up) = connect_fake_agent(h.grpc_addr, &token_up, v_below()).await.expect("upgrader 连接");
    let _first_up = stream_up.message().await.expect("recv").expect("msg");
    wait_until(|| async {
        h.app.state.agents.get_by_name("upgrader-1").await.unwrap().unwrap().online
    })
    .await;
    // 单台升级（REST）→ fake Agent 收 UpgradeCommand。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/agents/upgrader-1/upgrade",
        Some(format!(r#"{{"package_name": "{pkg}"}}"#)),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "升级指令 202");
    let cmd = collect_one_upgrade(&mut stream_up).await;
    assert_eq!(cmd.package_name, pkg);
    assert_eq!(cmd.sha256, pkg_sha);
    assert_eq!(cmd.download_url, format!("/api/v1/agent/upgrade-packages/{pkg}"));
    // 下载校验（agent token）→ sha256 头匹配。
    let dl = custom_req(&h.app, "GET", &cmd.download_url, None, None, &[("authorization", format!("Bearer {token_up}"))], DEFAULT_PEER).await;
    assert_eq!(dl.status(), axum::http::StatusCode::OK);
    assert_eq!(dl.headers().get("x-sisyphus-sha256").unwrap().to_str().unwrap(), pkg_sha);
    // 报 DRAINING → DOWNLOADING → SWAPPING → RESTARTING。
    upgrade_phase(&tx_up, UpgradePhase::UpgradeDraining).await;
    upgrade_phase(&tx_up, UpgradePhase::UpgradeDownloading).await;
    upgrade_phase(&tx_up, UpgradePhase::UpgradeSwapping).await;
    upgrade_phase(&tx_up, UpgradePhase::UpgradeRestarting).await;
    wait_until(|| async {
        h.app.state.agents.get_by_name("upgrader-1").await.unwrap().unwrap().upgrade_phase.as_deref() == Some("restarting")
    })
    .await;
    // fake「重启」：断开旧连接，以新版本 1.0.0 重连 → server 清升级态 + 落新版本。
    drop(tx_up);
    drop(stream_up);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut stream_up2, tx_up2) = connect_fake_agent(h.grpc_addr, &token_up, v_current()).await.expect("upgrader 新版本重连");
    let _first_up2 = stream_up2.message().await.expect("recv").expect("msg");
    wait_until(|| async {
        let row = h.app.state.agents.get_by_name("upgrader-1").await.unwrap().unwrap();
        row.upgrade_phase.is_none() && row.pending_upgrade.is_none()
    })
    .await;

    // 15. /metrics（票 #81 AC：含本次构建终态计数与时长样本）。
    // 等待终态计数落库（事件型指标在 store 层 transition 命中时记）。
    // 注意：metrics recorder 是进程级全局（与生产同形），同一测试二进制内多个
    // 用例共享计数——本主链路加了 2 个 succeeded，但失败路径用例可能也加过，
    // 故断言 >= 2（本次构建被计入）而非恰等 2。
    wait_until(|| async {
        let text = metrics_get(&h.app, &pat).await;
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"succeeded\"}").unwrap_or(0) >= 2
    })
    .await;
    let text = metrics_get(&h.app, &pat).await;
    assert!(
        text.contains("sisyphus_build_duration_seconds_bucket"),
        "构建时长直方图（bucket）出现：\n{text}"
    );
    assert!(
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"succeeded\"}").unwrap_or(0) >= 2,
        "本次两次成功构建应计入终态计数：\n{text}"
    );
    assert!(
        metric_value(&text, "sisyphus_agents_online").unwrap_or(0) >= 1,
        "至少一台 Agent 在线（gauge 为进程级当前值）：\n{text}"
    );

    // 16. 概览页全卡真值无退化（票 #81 AC）。
    // 先坐实两台 Agent 都在线（升级重连后 upgrader-1 须已注册在线），再断言
    // 概览 agents_online 真值——避免与重连异步的竞态。
    wait_until(|| async {
        let a = h.app.state.agents.get_by_name("linux-1").await.unwrap().unwrap().online;
        let b = h.app.state.agents.get_by_name("upgrader-1").await.unwrap().unwrap().online;
        a && b
    })
    .await;
    wait_until(|| async {
        let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&admin)).await;
        body_json(resp).await["builds_terminal"]["succeeded"].as_u64().unwrap_or(0) >= 2
    })
    .await;
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&admin)).await;
    let v = body_json(resp).await;
    assert_eq!(v["agents_online"].as_u64(), Some(2), "概览 Agent 在线数真值（两台）");
    assert_eq!(v["agents_total"].as_u64(), Some(2));
    assert_eq!(v["queue_depth"].as_u64(), Some(0), "队列已清空");
    assert_eq!(v["builds_terminal"]["succeeded"].as_u64(), Some(2), "概览终态计数真值");
    assert_eq!(v["alerts"]["has_offline_agent"].as_bool(), Some(false), "无离线 Agent 警示");
    assert_eq!(v["alerts"]["has_no_match"].as_bool(), Some(false), "无匹配任务警示归零");
    let recent = v["recent_builds"].as_array().expect("recent");
    assert!(recent.iter().any(|b| b["status"] == "succeeded"), "最近构建含成功条目");

    let _ = repo_dir;
    // 保活两台 Agent 的连接到用例结束（drop 即离线，概览 agents_online 失真）。
    drop(tx);
    drop(stream);
    drop(tx_up2);
    drop(stream_up2);
    h.grpc_handle.abort();
}

// ---------------------------------------------------------------------------
// SSE 读辅助（logs_pipeline.rs 同款）
// ---------------------------------------------------------------------------

/// 从 SSE 响应体增量读到谓词满足（拼接缓冲），或超时。返回全部文本与剩余流。
async fn read_sse_until(
    resp: axum::response::Response,
    pred: impl Fn(&str) -> bool,
) -> (String, axum::body::BodyDataStream) {
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("SSE 读超时");
        match chunk {
            Some(Ok(bytes)) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                if pred(&buf) {
                    return (buf, stream);
                }
            }
            Some(Err(e)) => panic!("SSE 流读失败：{e}"),
            None => return (buf, stream),
        }
    }
}

/// 续读既有 SSE 流到谓词满足或流结束。返回 true 若谓词满足，false 若流结束。
async fn continue_sse_until(
    stream: &mut axum::body::BodyDataStream,
    buf: &mut String,
    pred: impl Fn(&str) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("SSE 续读超时");
        match chunk {
            Some(Ok(bytes)) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                if pred(buf) {
                    return true;
                }
            }
            Some(Err(e)) => panic!("SSE 流读失败：{e}"),
            None => return false,
        }
    }
}

// ===========================================================================
// 失败路径 1：取消（排队 + 运行中）
// ===========================================================================

/// AC：取消排队中构建——无在线 Agent 时触发（任务排队）→ REST cancel → 排队
/// 任务出队、构建 cancelled、/metrics 终态计数 result=cancelled +1。
#[tokio::test]
async fn cancel_queued_build_drains_pending_and_marks_cancelled() {
    let h = harness().await;
    let (admin, _token) = seed_only(&h, "https://example.com/demo").await;
    let pat = create_pat(&h.app, &admin).await;

    // 触发（无在线 Agent → 任务排队）→ 202。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");

    // 排队中：REST cancel → 202 + cancelled。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds/1/cancel",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "取消 202");
    assert_eq!(body_json(resp).await["status"], "cancelled");
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Cancelled }).await;

    // /metrics：终态计数 result=cancelled +1。
    wait_until(|| async {
        let text = metrics_get(&h.app, &pat).await;
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"cancelled\"}").unwrap_or(0) >= 1
    })
    .await;
    h.grpc_handle.abort();
}

/// AC：取消运行中构建——fake Agent 在途（ack 后 running）→ REST cancel →
/// 通道下发 CancelBuild → 构建 cancelled（b2c 同款路径，本票收口断言）。
#[tokio::test]
async fn cancel_running_build_dispatches_cancel_to_agent() {
    let h = harness().await;
    let (admin, _token, mut stream, tx) = seed_and_connect(&h, "https://example.com/demo").await;
    // 触发 → fake Agent 收 JobSpec → ack（保持 running，不报终态）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");
    let trigger_body = body_json(resp).await;
    let build_id = trigger_body["build_id"].as_i64().expect("build_id");
    let spec = collect_one_job_spec(&mut stream).await;
    ack(&tx, &spec.job_id).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Running }).await;

    // REST 取消运行中 → 202。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds/1/cancel",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "取消 202");
    assert_eq!(body_json(resp).await["status"], "cancelled");

    // fake Agent 收 CancelBuild（经通道下发，build_id + job_id 命中）。
    let cancel = collect_one_cancel(&mut stream).await;
    assert_eq!(cancel.build_id, build_id.to_string(), "CancelBuild 命中构建");
    assert_eq!(cancel.job_id, spec.job_id, "CancelBuild 命中在途任务");

    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Cancelled }).await;
    h.grpc_handle.abort();
}

// ===========================================================================
// 失败路径 2：fail-fast 级联
// ===========================================================================

/// AC：fail-fast 级联——两任务同阶段，首任务 Failed（retry=0）→ 同阶段未完成
/// 任务 cancelled、构建 failed、/metrics result=failed +1。
#[tokio::test]
async fn fail_fast_cascades_sibling_job_on_failure() {
    let h = harness().await;
    let (admin, _token, mut stream, tx) = seed_and_connect_two_jobs(&h).await;
    let pat = create_pat(&h.app, &admin).await;

    // 触发 → 调度下发阶段 0 两任务（同阶段并发）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");
    let spec_a = collect_one_job_spec(&mut stream).await;
    let spec_b = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec_a.job_name, "compile");
    assert_eq!(spec_b.job_name, "package");
    ack(&tx, &spec_a.job_id).await;
    ack(&tx, &spec_b.job_id).await;

    // compile 报 Failed（retry=0）→ fail-fast 级联 package cancelled、构建 failed。
    report(&tx, &spec_a.job_id, JobPhase::JobFailed, Some(1)).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Failed }).await;

    // 构建详情：compile failed、package cancelled（级联）。
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/projects/demo/pipelines/release/builds/1", None, Some(&admin)).await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], "failed", "构建 failed");
    let jobs = body["stages"][0]["jobs"].as_array().expect("jobs");
    let compile = jobs.iter().find(|j| j["name"] == "compile").expect("compile 行");
    let package = jobs.iter().find(|j| j["name"] == "package").expect("package 行");
    assert_eq!(compile["status"], "failed", "compile failed");
    assert_eq!(package["status"], "cancelled", "package 级联 cancelled");

    // /metrics：result=failed +1。
    wait_until(|| async {
        let text = metrics_get(&h.app, &pat).await;
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"failed\"}").unwrap_or(0) >= 1
    })
    .await;
    h.grpc_handle.abort();
}

// ===========================================================================
// 失败路径 3：超时
// ===========================================================================

/// AC：超时路径——fake Agent 报 JobPhase::JobTimeout → JobStatus::Timeout（同
/// 失败类）→ 构建 failed、/metrics result=failed 或 result=timeout 计数 +1。
#[tokio::test]
async fn timeout_phase_marks_build_failed_and_metrics() {
    let h = harness().await;
    let (admin, _token, mut stream, tx) = seed_and_connect(&h, "https://example.com/demo").await;
    let pat = create_pat(&h.app, &admin).await;

    // 触发 → fake Agent 收 JobSpec → ack → 报 JobTimeout。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");
    let spec = collect_one_job_spec(&mut stream).await;
    ack(&tx, &spec.job_id).await;
    report(&tx, &spec.job_id, JobPhase::JobTimeout, None).await;
    // Timeout 同失败类 → fail-fast 级联（单任务即构建 failed）。
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Failed }).await;

    // 构建详情：任务 timeout（detail 记名）、构建 failed。
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/projects/demo/pipelines/release/builds/1", None, Some(&admin)).await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], "failed", "超时构建 failed");
    let job = body["stages"][0]["jobs"].as_array().expect("jobs")[0].clone();
    assert_eq!(job["status"], "timeout", "任务终态 timeout（ADR-0008 同失败类映射）");

    // /metrics：失败类终态计数 +1。
    wait_until(|| async {
        let text = metrics_get(&h.app, &pat).await;
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"failed\"}").unwrap_or(0) >= 1
            || metric_value(&text, "sisyphus_builds_terminal_total{result=\"timeout\"}").unwrap_or(0) >= 1
    })
    .await;
    h.grpc_handle.abort();
}

// ===========================================================================
// 失败路径 4：重跑（从头 + 失败任务 attempt+1）
// ===========================================================================

/// AC：重跑——from_scratch（新构建号 attempt=1）+ from_failed（同号 attempt+1）。
/// from_failed 已由 b2c 覆盖；本票补 from_scratch 一条并断言构建号递增。
#[tokio::test]
async fn rerun_from_scratch_new_number_and_from_failed_attempt_plus_one() {
    let h = harness().await;
    let (admin, _token, mut stream, tx) = seed_and_connect(&h, "https://example.com/demo").await;

    // 触发构建 1 → Failed（retry=0，fail-fast）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds",
        Some("{}".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "触发 202");
    let spec = collect_one_job_spec(&mut stream).await;
    ack(&tx, &spec.job_id).await;
    report(&tx, &spec.job_id, JobPhase::JobFailed, Some(1)).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Failed }).await;

    // from_scratch：新构建号 2、attempt=1、复制原触发上下文。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds/1/rerun",
        Some(r#"{"mode":"from_scratch"}"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "from_scratch 重跑 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 2, "from_scratch 新号 2");
    assert_eq!(body["attempt"], 1, "from_scratch attempt=1");
    // 调度下发构建 2：fake Agent 收新 JobSpec（attempt=1）→ Succeeded。
    let spec2 = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec2.build_number, 2);
    assert_eq!(spec2.attempt, 1);
    ack(&tx, &spec2.job_id).await;
    report(&tx, &spec2.job_id, JobPhase::JobSucceeded, Some(0)).await;
    wait_until(|| async { build_status(&h, 2).await == BuildStatus::Succeeded }).await;

    // 构建详情 2 → succeeded（新号独立终态，构建 1 仍 failed）。
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/projects/demo/pipelines/release/builds/2", None, Some(&admin)).await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], "succeeded", "from_scratch 新构建 succeeded");
    assert_eq!(body["number"], 2);
    // 构建 1 历史 failed 保留。
    assert_eq!(build_status(&h, 1).await, BuildStatus::Failed, "构建 1 failed 历史保留");

    // from_failed：对构建 1（failed）重跑 → 同号 attempt+1（b2c 同款，收口断言）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds/1/rerun",
        Some(r#"{"mode":"from_failed"}"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "from_failed 重跑 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1, "from_failed 同号延续");
    assert_eq!(body["attempt"], 2, "from_failed attempt+1");
    // 调度下发构建 1 attempt=2 → Succeeded。
    let spec3 = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec3.build_number, 1);
    assert_eq!(spec3.attempt, 2, "from_failed 下发 attempt=2");
    ack(&tx, &spec3.job_id).await;
    report(&tx, &spec3.job_id, JobPhase::JobSucceeded, Some(0)).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Succeeded }).await;

    // 构建详情 1 → succeeded；attempt 1（failed）与 2（succeeded）并存。
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/projects/demo/pipelines/release/builds/1", None, Some(&admin)).await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], "succeeded", "from_failed 续跑至成功");
    let jobs = body["stages"][0]["jobs"].as_array().expect("jobs");
    let attempts: Vec<i64> = jobs.iter().map(|j| j["attempt"].as_i64().expect("attempt")).collect();
    assert!(attempts.contains(&1) && attempts.contains(&2), "attempt 1/2 并存：{attempts:?}");

    // /metrics：两条重跑路径各贡献一个 succeeded 终态计数（票 #81 AC：/metrics
    // 在 e2e 结束时含本次构建终态计数）。metrics 为进程级全局，故断言增量 >= 2。
    let pat = create_pat(&h.app, &admin).await;
    wait_until(|| async {
        let text = metrics_get(&h.app, &pat).await;
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"succeeded\"}").unwrap_or(0) >= 2
    })
    .await;
    let text = metrics_get(&h.app, &pat).await;
    assert!(
        metric_value(&text, "sisyphus_builds_terminal_total{result=\"succeeded\"}").unwrap_or(0) >= 2,
        "重跑两条路径的 succeeded 终态计数应计入 /metrics：\n{text}"
    );

    h.grpc_handle.abort();
}

// ---------------------------------------------------------------------------
// 前置变体：两任务同阶段（fail-fast 面）
// ---------------------------------------------------------------------------

/// 两任务同阶段 pipeline（fail-fast 面）：compile + package，labels=os=linux，retry=0。
fn pipeline_two_jobs() -> Pipeline {
    let job = |name: &str| Job {
        name: name.into(),
        exec_env: None,
        labels: vec!["sisyphus/os=linux".into()],
        when: None,
        env: vec![],
        allow_failure: false,
        retry_count: 0,
        timeout_minutes: 0,
        artifact_uploads: vec![],
        artifact_downloads: vec![],
        caches: vec![],
        secrets: vec![],
        steps: vec![Step::Shell {
            command: "echo hi".into(),
            shell: None,
            when: None,
        }],
    };
    Pipeline {
        name: "release".into(),
        parameters: vec![],
        env: vec![],
        notification: None,
        stages: vec![Stage {
            name: "build".into(),
            when: None,
            jobs: vec![job("compile"), job("package")],
        }],
        revision: None,
    }
}

/// 建项目 + 存两任务定义 + 建 Agent + 连接 fake Agent（fail-fast 面）。
async fn seed_and_connect_two_jobs(h: &Harness) -> (String, String, tonic::Streaming<ChannelMessage>, mpsc::Sender<ChannelMessage>) {
    let admin = common::setup_and_login(&h.app).await;
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects",
        Some(r#"{"name":"demo","scm_type":"git","scm_url":"https://example.com/demo","default_branch":"main"}"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建项目");
    let resp = req_with_cookie(
        &h.app,
        "PUT",
        "/api/v1/projects/demo/pipelines/release",
        Some(serde_json::to_string(&pipeline_two_jobs()).expect("序列化定义")),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "存定义");
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/agents",
        Some(r#"{ "name": "linux-1", "max_concurrency": 2 }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建 Agent");
    let token = body_json(resp).await["token"].as_str().expect("token").to_string();
    // 与主链路构建 Agent 同版本（1.0.0，可派发）；本面不做升级往返，版本无关。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token, v_current()).await.expect("连接");
    let first = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))), "握手回执");
    (admin, token, stream, tx)
}

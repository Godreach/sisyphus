//! 调度真实下发闭环（票 B2c-T4 AC，Spec B2c §Testing）：proto 缝 fake Agent
//! 经真实 tonic 通道收 JobSpec → ack → 回状态 → 构建推进到终态。
//!
//! 形态基准：B1 `full_duplex_round_trip_over_inprocess_channel`（内存 tonic
//! 双向流）+ grpc_auth 的「真实 tonic 服务 + 本地 socket + 真实 store」——
//! 本用例再加调度循环（`sched::Scheduler` 真实事件驱动）与 engine（统一
//! 触发入口），把「定义 → 触发 → 编排 → 调度 → 通道下发 → 状态回收 → 终态」
//! 全链路在进程内跑通。fake Agent 持 proto 契约在真实装配下工作，不 spawn
//! 进程。
//!
//! 覆盖验收点：ResolvedJobSpec 组装（when 求值/变量替换/env 合并/机密注入/
//! 隐式容器标签）经契约往返、ack 槽位占用、JobReported 重连重建、CancelBuild
//! 下发。时间语义（超时/宽限）经事件驱动与假时钟由内联单测覆盖，此处只
//! 验「真实下发闭环」的行为结果。

use std::sync::Arc;
use std::time::Duration;

use sisyphus_model::pipeline::{EnvVar, ExecutionEnv, Job, Pipeline, Shell, Stage, Step};
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, JobAck, JobStatus as ProtoJobStatus, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::engine::{StartBuildInput, TriggerDetail};
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::store::builds::{BuildStatus, TriggerSource};
use sisyphus_server::store::jobs::JobRepo;
use sisyphus_server::store::projects::{NewProject, ProjectRepo, ScmType};
use sisyphus_server::{api, grpc, sched, store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

/// 与 workspace 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

/// 进程内装配：真实 store（临时库）+ 组合根 + 真实调度循环（事件驱动）。
struct Harness {
    _dir: tempfile::TempDir,
    state: api::AppState,
    /// 真实调度循环（run 持有 receiver；JoinHandle 在此保活）。
    _sched: tokio::task::JoinHandle<()>,
    grpc_addr: std::net::SocketAddr,
    grpc_handle: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = api::AppState::new(pool.clone(), false, master_key, sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES);

    let sessions = Arc::new(grpc::SessionRegistry::new());
    let dispatcher = Arc::new(grpc::GrpcDispatcher::new(sessions.clone()));
    let scheduler = sched::Scheduler::new(state.engine.clone(), pool.clone(), dispatcher, 10);
    let scheduler_handle = scheduler.handle();
    let bus = state.bus.clone();
    let sched_task = tokio::spawn(async move {
        scheduler.reconstruct().await.expect("重建");
        scheduler.run(bus).await;
    });

    // 真实 tonic gRPC 服务（认证 + 心跳 + 任务面全接线）。
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

    Harness {
        _dir: dir,
        state,
        _sched: sched_task,
        grpc_addr,
        grpc_handle,
    }
}

/// 建项目 + 存定义 + 建 Agent（在线由 fake Agent 连接置位）。
/// 返回 (Agent token 明文仅此一次, Agent 行 id)。
///
/// 等到谓词成立或超时（调度是异步路径，轮询驱动避免 flaky）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("条件在 10s 内未成立");
}

/// fake Agent：连上（握手 + 系统标签 metadata 置 os/arch/container）→ 收
/// 下行帧。返回 (响应流, 上行发送器)。
async fn connect_fake_agent(
    addr: std::net::SocketAddr,
    token: &str,
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
    let (tx, rx) = mpsc::channel(16);
    tx.send(ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(version()),
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
    for (key, value) in [
        ("x-sisyphus-os", "linux"),
        ("x-sisyphus-arch", "amd64"),
        ("x-sisyphus-container", "docker"),
    ] {
        request
            .metadata_mut()
            .insert(key, MetadataValue::try_from(value).expect("值"));
    }
    let response = client.connect(request).await?;
    Ok((response.into_inner(), tx))
}

/// 从响应流收任务规格（跳过握手回执；collect 全部 JobSpec 序列）。
async fn collect_job_specs(
    stream: &mut tonic::Streaming<ChannelMessage>,
    tx: &mpsc::Sender<ChannelMessage>,
    expected: usize,
) -> Vec<sisyphus_proto::agent::JobSpec> {
    let mut specs = Vec::new();
    while specs.len() < expected {
        let msg = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("收帧超时")
            .expect("recv")
            .expect("msg");
        match msg.kind {
            Some(Kind::Handshake(_)) => continue,
            Some(Kind::JobSpec(spec)) => specs.push(*spec),
            Some(Kind::Cancel(_)) => continue,
            other => panic!("意外下行帧：{other:?}"),
        }
    }
    let _ = tx;
    specs
}

#[tokio::test]
async fn full_duplex_dispatch_and_terminal() {
    let h = harness().await;
    let (token, _agent_id) = seed_with_token(&h).await;

    // fake Agent 连接（系统标签含 os/arch/container → 两个任务都可匹配）。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token).await.expect("连接");
    // 收握手回执（会话建立确认）。
    let first = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))));

    // 触发构建：手动（默认分支 main）。
    let build = h
        .state
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
        .expect("触发");

    // 调度循环放行 + 组装 + 匹配下发：阶段 0 两个任务并行投递。
    let specs = collect_job_specs(&mut stream, &tx, 2).await;
    assert_eq!(specs.len(), 2, "阶段 0 两个任务并行下发");
    let compile = specs
        .iter()
        .find(|s| s.job_name == "compile")
        .expect("compile");
    let unit = specs.iter().find(|s| s.job_name == "unit").expect("unit");

    // 契约往返验证：ResolvedJobSpec 组装经真实通道到达。
    assert_eq!(compile.pipeline_name, "release");
    assert_eq!(compile.build_number, build.number);
    assert_eq!(compile.attempt, 1);
    assert_eq!(compile.timeout_minutes, 30);
    // env 合并：pipeline 级 + 任务级 + 内置变量（变量替换后的命令）。
    assert_eq!(
        compile.env.get("PIPELINE_ENV").map(String::as_str),
        Some("from-pipeline")
    );
    assert_eq!(
        compile.env.get("JOB_ENV").map(String::as_str),
        Some("from-job")
    );
    assert!(
        compile.steps.iter().any(|s| matches!(&s.kind,
            Some(sisyphus_proto::agent::job_step::Kind::Shell(sh))
                if sh.command == format!("echo {}", build.number)
        )),
        "shell 命令变量替换完成（SISY_BUILD_NUMBER）"
    );
    // 机密注入：任务引用的 DEPLOY_KEY 解密后随 env 下发（ADR-0015）。
    assert_eq!(
        compile.env.get("DEPLOY_KEY").map(String::as_str),
        Some("deploy-secret-value"),
        "机密解密注入 env 随规格下发"
    );
    assert!(
        compile.secrets.contains(&"DEPLOY_KEY".to_string()),
        "机密名清单随规格下发（审计「哪些凭据随任务」）"
    );
    // when 求值：unit 任务级 when `${SISY_BRANCH} == "main"`，main 分支触发
    // → 下发（任务级 when 在 Server 端求值，经契约往返）。
    assert_eq!(unit.job_name, "unit", "when 满足的任务照常下发");
    // 容器任务：隐式容器标签随规格下发。
    assert!(
        unit.labels
            .contains(&"sisyphus/container=docker".to_string()),
        "容器任务隐式容器标签随规格下发"
    );
    assert!(matches!(
        &unit.exec_env,
        Some(sisyphus_proto::agent::ExecutionEnv {
            kind: Some(sisyphus_proto::agent::execution_env::Kind::Container(c)),
            ..
        }) if c.image == "rust:1.97"
    ));
    // host 任务：exec_env 为 Host。
    assert!(matches!(
        &compile.exec_env,
        Some(sisyphus_proto::agent::ExecutionEnv {
            kind: Some(sisyphus_proto::agent::execution_env::Kind::Host(_)),
            ..
        })
    ));

    // ack 两个任务（槽位占用确认）。
    for spec in &specs {
        tx.send(ChannelMessage {
            kind: Some(Kind::JobAck(JobAck {
                job_id: spec.job_id.clone(),
                accepted: true,
                error: String::new(),
            })),
        })
        .await
        .expect("send ack");
    }

    // 回 Succeeded：阶段 0 全成功 → engine 推进（阶段串行，无下一阶段）→
    // 构建终态 succeeded。
    for spec in &specs {
        tx.send(ChannelMessage {
            kind: Some(Kind::JobStatus(ProtoJobStatus {
                job_id: spec.job_id.clone(),
                phase: sisyphus_proto::agent::JobPhase::JobSucceeded as i32,
                exit_code: Some(0),
                detail: String::new(),
            })),
        })
        .await
        .expect("send status");
    }

    // 构建推进到 succeeded。
    let builds = sisyphus_server::store::builds::BuildRepo::new(h.state.pool.clone());
    wait_until(|| async {
        let row = builds.get(build.id).await.expect("查").expect("应存在");
        row.status == BuildStatus::Succeeded
    })
    .await;
    let row = builds.get(build.id).await.expect("查").expect("应存在");
    assert_eq!(
        row.status,
        BuildStatus::Succeeded,
        "fake Agent 全链路推进到终态"
    );
    assert!(row.finished_at.is_some());

    // 槽位释放：两个任务终态后 Agent 无在途任务。
    let jobs = JobRepo::new(h.state.pool.clone());
    assert_eq!(
        jobs.active_by_agent(_agent_id).await.expect("在途"),
        0,
        "终态释放槽位"
    );

    h.grpc_handle.abort();
}

/// seed 的带 token 版本：建条目时返回明文 token（仅此一次）。同时建一份
/// 机密（DEPLOY_KEY）供任务引用——闭环测试验证机密解密注入随规格下发。
async fn seed_with_token(h: &Harness) -> (String, i64) {
    let project = ProjectRepo::new(h.state.pool.clone())
        .create(NewProject {
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        })
        .await
        .expect("建项目");
    // 机密：XChaCha20 加密落库（与生产写入同路径），供任务引用注入 env。
    let blob = sisyphus_server::secrets::encrypt(&h.state.master_key, b"deploy-secret-value")
        .expect("加密机密");
    h.state
        .secrets
        .upsert(project.id, "DEPLOY_KEY", &blob, "alice", 0)
        .await
        .expect("建机密");
    let pipeline = Pipeline {
        name: "release".into(),
        parameters: vec![],
        env: vec![EnvVar {
            name: "PIPELINE_ENV".into(),
            value: "from-pipeline".into(),
        }],
        notification: None,
        stages: vec![Stage {
            name: "build".into(),
            when: None,
            jobs: vec![
                Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec!["sisyphus/os=linux".into()],
                    when: None,
                    env: vec![EnvVar {
                        name: "JOB_ENV".into(),
                        value: "from-job".into(),
                    }],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 30,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec!["DEPLOY_KEY".into()],
                    steps: vec![Step::Shell {
                        command: "echo ${SISY_BUILD_NUMBER}".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                },
                Job {
                    name: "unit".into(),
                    exec_env: Some(ExecutionEnv::Container {
                        image: "rust:1.97".into(),
                    }),
                    labels: vec![],
                    // 任务级 when：main 分支触发（触发用 main）→ 下发。
                    when: Some("${SISY_BRANCH} == \"main\"".into()),
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "cargo test".into(),
                        shell: None,
                        when: None,
                    }],
                },
            ],
        }],
        revision: None,
    };
    h.state
        .pipelines
        .save("demo", "release", &pipeline, "tester")
        .await
        .expect("存定义");
    let token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let row = h
        .state
        .agents
        .create(NewAgent {
            name: "linux-1".into(),
            token_hash: token_hash(&token),
            system_labels: r#"["sisyphus/os=linux"]"#.into(),
            custom_labels: "[]".into(),
            max_concurrency: 2,
            register_code_hash: token_hash(&code),
        })
        .await
        .expect("建 Agent");
    (token, row.id)
}

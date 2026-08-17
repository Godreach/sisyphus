//! Spec B2c tracer bullet 全链路（票 #49 AC）：Router 缝（REST oneshot）+
//! proto 缝（fake Agent 经真实 tonic 通道）+ 真实调度循环（事件驱动）共享
//! 同一 AppState。setup → 建项目存定义 → 建 Agent → 手动触发 → engine 求值
//! 组装 → 调度下发 → fake Agent 收 JobSpec ack → 回状态 → 构建推进到终态 →
//! 构建详情可查 → 从失败任务重跑 attempt+1 → 续跑至成功。
//!
//! 形态基准：`sched_closed_loop`（真实 tonic + 真实 store + 调度循环）+
//! `tracer_bullet`（Router 缝 oneshot）——本用例把 REST 触发/详情/重跑与
//! 调度下发 / fake Agent 收发帧在同一进程内串成一条链。不起独立进程。

use std::sync::Arc;
use std::time::Duration;

use sisyphus_model::pipeline::{EnvVar, Job, Pipeline, Stage, Step};
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, JobAck, JobPhase, JobStatus as ProtoJobStatus, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_server::api::AppState;
use sisyphus_server::store::builds::{BuildRepo, BuildStatus};
use sisyphus_server::{grpc, sched, store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::metadata::MetadataValue;

mod common;

use common::{TestApp, body_json, req_with_cookie, test_app_from_state};

/// 与 workspace 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

/// 进程内装配：真实 store + 组合根 + REST router + 真实调度循环 + 真实 gRPC。
struct Harness {
    _dir: tempfile::TempDir,
    app: TestApp,
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
    let state = AppState::new(pool.clone(), false, master_key);
    // REST router 与 scheduler/gRPC 共享同一 state（事件总线串起触发→下发）。
    let app = test_app_from_state(state.clone(), dir.path());

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
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });

    Harness {
        _dir: dir,
        app,
        _sched: sched_task,
        grpc_addr,
        grpc_handle,
    }
}

/// 等到谓词成立或超时（调度是异步路径，轮询驱动避免 flaky）。
async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("条件在 10s 内未成立");
}

/// 单任务 pipeline：labels 指定 sisyphus/os=linux（匹配 fake Agent 系统标签），
/// retry_count=0（首败即 fail-fast），无机密（fake Agent 直接报成败）。
fn pipeline() -> Pipeline {
    Pipeline {
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
            jobs: vec![Job {
                name: "compile".into(),
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
                    command: "echo ${SISY_BUILD_NUMBER}".into(),
                    shell: None,
                    when: None,
                }],
            }],
        }],
        revision: None,
    }
}

/// fake Agent 连接（握手 + 系统标签 metadata）→ 返回 (响应流, 上行发送器)。
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
    // 系统标签：os=linux（匹配任务 labels）。
    request.metadata_mut().insert(
        "x-sisyphus-os",
        MetadataValue::try_from("linux").expect("值"),
    );
    let response = client.connect(request).await?;
    Ok((response.into_inner(), tx))
}

/// 从响应流收一条 JobSpec（跳过握手回执 / Cancel 帧）。
async fn collect_one_job_spec(
    stream: &mut tonic::Streaming<ChannelMessage>,
) -> sisyphus_proto::agent::JobSpec {
    for _ in 0..200 {
        let msg = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("收帧超时")
            .expect("recv")
            .expect("msg");
        if let Some(Kind::JobSpec(spec)) = msg.kind {
            return *spec;
        }
        // 握手回执 / Cancel 帧跳过（本用例单任务、无取消下发）。
    }
    panic!("10s 内未收到 JobSpec");
}

/// 从响应流收一条 CancelBuild（跳过握手回执 / JobSpec 帧）。
async fn collect_one_cancel(
    stream: &mut tonic::Streaming<ChannelMessage>,
) -> sisyphus_proto::agent::CancelBuild {
    for _ in 0..200 {
        let msg = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .expect("收帧超时")
            .expect("recv")
            .expect("msg");
        if let Some(Kind::Cancel(cancel)) = msg.kind {
            return cancel;
        }
    }
    panic!("10s 内未收到 CancelBuild");
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

/// 发 JobStatus（指定阶段）。
async fn report(tx: &mpsc::Sender<ChannelMessage>, job_id: &str, phase: JobPhase) {
    tx.send(ChannelMessage {
        kind: Some(Kind::JobStatus(ProtoJobStatus {
            job_id: job_id.into(),
            phase: phase as i32,
            exit_code: Some(if matches!(phase, JobPhase::JobSucceeded) { 0 } else { 1 }),
            detail: String::new(),
        })),
    })
    .await
    .expect("send status");
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

/// Spec B2c tracer bullet：触发 → 终态 → 从失败重跑 attempt+1 → 续跑至成功。
#[tokio::test]
async fn b2c_tracer_bullet_full_chain() {
    let h = harness().await;

    // 1. setup wizard（Router 缝）→ admin cookie。
    let admin = common::setup_and_login(&h.app).await;

    // 2. 建项目（REST admin）。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/demo" }"#
                .into(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED, "建项目");

    // 3. 存定义（REST admin）——单任务、labels=os=linux、retry=0。
    let resp = req_with_cookie(
        &h.app,
        "PUT",
        "/api/v1/projects/demo/pipelines/release",
        Some(serde_json::to_string(&pipeline()).expect("序列化定义")),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "存定义");

    // 4. 建 Agent（REST admin）→ token 明文仅此一次。
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

    // 5. fake Agent 连接（系统标签 os=linux → 在线 + 标签匹配）。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token)
        .await
        .expect("连接");
    let first = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))), "握手回执");

    // 6. 手动触发（REST admin）→ 202 + 构建号 1。
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

    // 7. 调度循环放行 + 组装 + 匹配下发：fake Agent 收 JobSpec（attempt=1）。
    let spec = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec.job_name, "compile");
    assert_eq!(spec.attempt, 1, "首次下发 attempt=1");
    assert_eq!(spec.build_number, 1);
    assert_eq!(spec.pipeline_name, "release");
    // 契约往返：env 合并（pipeline 级变量替换后下发）。
    assert_eq!(
        spec.env.get("PIPELINE_ENV").map(String::as_str),
        Some("from-pipeline"),
        "env 经真实通道到达"
    );

    // 8. fake Agent 回执 + 报 Failed → 构建 failed（fail-fast，retry=0）。
    ack(&tx, &spec.job_id).await;
    report(&tx, &spec.job_id, JobPhase::JobFailed).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Failed }).await;

    // 9. 构建详情可查（REST admin）→ failed。
    let resp = req_with_cookie(
        &h.app,
        "GET",
        "/api/v1/projects/demo/pipelines/release/builds/1",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK, "详情 200");
    let body = body_json(resp).await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["trigger_by"], "admin", "触发人实名");
    assert_eq!(body["stages"][0]["name"], "build");

    // 10. 从失败任务重跑（REST admin）→ 同号 attempt+1。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects/demo/pipelines/release/builds/1/rerun",
        Some(r#"{ "mode": "from_failed" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED, "重跑 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1, "同号延续");
    assert_eq!(body["attempt"], 2, "attempt+1");

    // 11. 调度重开失败任务：fake Agent 收新 JobSpec（attempt=2）。
    let spec2 = collect_one_job_spec(&mut stream).await;
    assert_eq!(spec2.job_name, "compile");
    assert_eq!(spec2.attempt, 2, "重跑下发 attempt=2");

    // 12. fake Agent 回 Succeeded → 构建 succeeded。
    ack(&tx, &spec2.job_id).await;
    report(&tx, &spec2.job_id, JobPhase::JobSucceeded).await;
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Succeeded }).await;

    // 13. 构建详情 → succeeded；任务 attempt=1（failed）与 attempt=2（succeeded）
    //     并存（重跑保留失败历史，ADR-0006 重跑语义）。
    let resp = req_with_cookie(
        &h.app,
        "GET",
        "/api/v1/projects/demo/pipelines/release/builds/1",
        None,
        Some(&admin),
    )
    .await;
    let body = body_json(resp).await;
    assert_eq!(body["status"], "succeeded", "重跑续跑至成功");
    let jobs = body["stages"][0]["jobs"].as_array().expect("jobs");
    let compile_rows: Vec<&serde_json::Value> = jobs
        .iter()
        .filter(|j| j["name"] == "compile")
        .collect();
    let attempts: Vec<i64> = compile_rows
        .iter()
        .map(|j| j["attempt"].as_i64().expect("attempt"))
        .collect();
    assert!(
        attempts.contains(&1) && attempts.contains(&2),
        "attempt 1（failed）与 2（succeeded）并存：{attempts:?}"
    );
    let a1 = compile_rows
        .iter()
        .find(|j| j["attempt"] == 1)
        .expect("attempt 1 行");
    assert_eq!(a1["status"], "failed", "attempt 1 失败历史保留");
    let a2 = compile_rows
        .iter()
        .find(|j| j["attempt"] == 2)
        .expect("attempt 2 行");
    assert_eq!(a2["status"], "succeeded", "attempt 2 重跑成功");

    h.grpc_handle.abort();
}

/// AC：POST 取消运行中构建经通道下发 CancelBuild——REST cancel → engine
/// DB 迁移 + 发 BuildStatus{Cancelled} 事件 → sched 经通道向在途任务下发
/// CancelBuild（与 fail-fast 同款事件路径），fake Agent 收到取消帧。
#[tokio::test]
async fn rest_cancel_running_build_dispatches_cancel_to_agent() {
    let h = harness().await;
    let admin = common::setup_and_login(&h.app).await;

    // 建项目 + 存定义 + 建 Agent。
    let resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/demo" }"#
                .into(),
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

    // 连 fake Agent、收握手回执。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token)
        .await
        .expect("连接");
    let first = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(first.kind, Some(Kind::Handshake(_))));

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
    // 构建进 running（job 在途、占槽）。
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Running }).await;

    // REST 取消运行中构建 → 202。
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

    // 构建置 cancelled。
    wait_until(|| async { build_status(&h, 1).await == BuildStatus::Cancelled }).await;

    h.grpc_handle.abort();
}

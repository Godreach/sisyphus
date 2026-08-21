//! 概览快照 + `/metrics` 双消费进程内集成（票 B5-T7 AC，ADR-0019）：
//! 真实 store + 真实调度循环 + 真实 gRPC（fake Agent 全链路推进构建到终态），
//! 断言 `/api/v1/overview` 的 stat 卡真值与警示态、`/metrics` Prometheus 文本
//! 的指标增量与鉴权开关两态（`[metrics] auth` 默认开 → 401，关 → 公开）。
//!
//! 形态基准：`sched_closed_loop`（真实调度 + 真实 gRPC + fake Agent 全链路）
//! 与 REST 组合根（`common::TestApp`）合流——同一 AppState 喂 REST 面与
//! 调度/gRPC 面（生产 main.rs 同源装配）。fake Agent 持 proto 契约在真实
//! 装配下工作，不 spawn 进程。
//!
//! 覆盖验收点：概览快照=DB 真值（队列原因分类 / Agent 在线数 / 槽位占用 /
//! 终态计数 / 警示态 / 最近构建）经 REST 往返；调度周期灌入的当前值 + 事件
//! 埋点的终态计数出现在 `/metrics` 文本；鉴权两态。

use std::sync::Arc;
use std::time::Duration;

use axum::http::{StatusCode, header};
use sisyphus_model::pipeline::{Pipeline, Stage, Step};
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, JobAck, JobStatus as ProtoJobStatus, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::engine::{StartBuildInput, TriggerDetail};
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::store::builds::{BuildStatus, TriggerSource};
use sisyphus_server::store::projects::{NewProject, ProjectRepo, ScmType};
use sisyphus_server::{api, grpc, sched, store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

mod common;

use common::{
    TestApp, body_json, body_text, req_with_cookie, setup_and_login, test_app_from_state,
};

/// 与 workspace 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

/// 进程内装配：真实 store（临时库）+ REST 组合根 + 真实调度循环 + 真实 gRPC。
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
    let state = api::AppState::new(
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
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("条件在 10s 内未成立");
}

/// 建项目 + 存 pipeline 定义 + 建 Agent（不连接）。返回 (token 明文, agent 行 id)。
async fn seed_project_pipeline_agent(h: &Harness) -> (String, i64) {
    let _project = ProjectRepo::new(h.app.state.pool.clone())
        .create(NewProject {
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        })
        .await
        .expect("建项目");
    let pipeline = Pipeline {
        name: "release".into(),
        parameters: vec![],
        env: vec![],
        notification: None,
        stages: vec![Stage {
            name: "build".into(),
            when: None,
            jobs: vec![sisyphus_model::pipeline::Job {
                name: "compile".into(),
                exec_env: None,
                labels: vec!["sisyphus/os=linux".into()],
                when: None,
                env: vec![],
                allow_failure: false,
                retry_count: 0,
                timeout_minutes: 30,
                artifact_uploads: vec![],
                artifact_downloads: vec![],
                caches: vec![],
                secrets: vec![],
                steps: vec![Step::Shell {
                    command: "echo ok".into(),
                    shell: None,
                    when: None,
                }],
            }],
        }],
        revision: None,
    };
    h.app
        .state
        .pipelines
        .save("demo", "release", &pipeline, "tester")
        .await
        .expect("存定义");
    let token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let row = h
        .app
        .state
        .agents
        .create(NewAgent {
            name: "linux-1".into(),
            token_hash: token_hash(&token),
            system_labels: r#"["sisyphus/os=linux"]"#.into(),
            custom_labels: "[]".into(),
            max_concurrency: 2,
            register_code_hash: token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建 Agent");
    (token, row.id)
}

/// fake Agent：连上（握手 + 系统标签 metadata 置 os/arch/container）→ 收下行帧。
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

/// 从响应流收任务规格（跳过握手回执；收齐 expected 个 JobSpec）。
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

/// 从 `/metrics` 文本提取单行指标值（按行前缀匹配）。
fn metric_value(text: &str, prefix: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        name.starts_with(prefix)
            .then(|| value.parse().ok())
            .flatten()
    })
}

/// Bearer 认证的进程内请求（/metrics 鉴权面）。
async fn bearer_get(app: &TestApp, path: &str, token: &str) -> axum::response::Response {
    common::custom_req(
        app,
        "GET",
        path,
        None,
        None,
        &[("authorization", format!("Bearer {token}"))],
        common::DEFAULT_PEER,
    )
    .await
}

#[tokio::test]
async fn overview_and_metrics_report_true_state() {
    let h = harness().await;
    // 登录（首个 admin）→ cookie；建 PAT（/metrics Bearer 鉴权面）。
    let cookie = setup_and_login(&h.app).await;
    let pat_resp = req_with_cookie(
        &h.app,
        "POST",
        "/api/v1/auth/tokens",
        Some(serde_json::json!({ "name": "metrics" }).to_string()),
        Some(&cookie),
    )
    .await;
    assert_eq!(pat_resp.status(), StatusCode::CREATED, "建 PAT 应 201");
    let pat = body_json(pat_resp).await["token"]
        .as_str()
        .expect("token")
        .to_string();

    // 未建任何东西：概览是空态真值。
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "登录可读概览");
    let v = body_json(resp).await;
    assert_eq!(v["queue_depth"].as_u64(), Some(0));
    assert_eq!(v["agents_online"].as_u64(), Some(0));
    assert_eq!(v["agents_total"].as_u64(), Some(0));
    assert_eq!(v["slots_used"].as_u64(), Some(0));
    assert_eq!(v["slots_total"].as_u64(), Some(0));
    assert_eq!(v["builds_terminal"]["succeeded"].as_u64(), Some(0));
    assert_eq!(v["artifact_bytes"].as_u64(), Some(0));
    assert_eq!(v["log_bytes"].as_u64(), Some(0));
    assert_eq!(v["alerts"]["has_offline_agent"].as_bool(), Some(false));
    assert_eq!(v["recent_builds"].as_array().map(Vec::len), Some(0));

    // /metrics 鉴权：默认开 → 无凭证 401；PAT 通过且文本出现契约名。
    let anon = common::get(&h.app, "/metrics").await;
    assert_eq!(
        anon.status(),
        StatusCode::UNAUTHORIZED,
        "metrics 默认需认证"
    );
    let auth_metrics = bearer_get(&h.app, "/metrics", &pat).await;
    assert_eq!(auth_metrics.status(), StatusCode::OK);
    let text = body_text(auth_metrics).await;
    for name in [
        "sisyphus_queue_depth",
        "sisyphus_agents_online",
        "sisyphus_agents_total",
        "sisyphus_slots_used",
        "sisyphus_slots_total",
        "sisyphus_storage_bytes",
        "sisyphus_scheduler_last_activity_ms",
    ] {
        assert!(text.contains(name), "{name} 应在 /metrics：\n{text}");
    }
    // 概览空态已把当前值灌入 recorder（report_snapshot 固定标签全集）。
    assert!(
        text.contains("reason=\"no_online_agent\""),
        "队列原因维度稳定输出"
    );
    // 空态下无任何终态事件：终态计数器尚未被触碰（Prometheus 只输出已触碰
    // 指标）——作为增量断言基线，先记「不存在」。
    assert!(
        metric_value(
            &text,
            "sisyphus_builds_terminal_total{result=\"succeeded\"}"
        )
        .is_none(),
        "空态不应有终态计数行"
    );

    // 建项目 + pipeline + Agent（不连接）：Agent 离线 → 警示态成立。
    let (token, _agent_id) = seed_project_pipeline_agent(&h).await;
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
    let v = body_json(resp).await;
    assert_eq!(v["agents_total"].as_u64(), Some(1));
    assert_eq!(v["agents_online"].as_u64(), Some(0));
    assert_eq!(v["alerts"]["has_offline_agent"].as_bool(), Some(true));

    // 触发构建（无在线 Agent）：任务入队并标注等待原因（无在线 Agent）。
    let build = h
        .app
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
    wait_until(|| async {
        let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
        let v = body_json(resp).await;
        v["queue_depth"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
    let v = body_json(resp).await;
    assert_eq!(v["queue_depth"].as_u64(), Some(1));
    let no_online = v["queue_reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .find(|r| r["reason"] == "no_online_agent")
        .expect("no_online_agent 分类");
    assert_eq!(no_online["depth"].as_u64(), Some(1));
    assert_eq!(v["alerts"]["has_no_match"].as_bool(), Some(true));
    // 未到终态：构建已被驱动到 running（任务在队列等待 Agent），最近构建列表
    // 里是 running 条目（构建生命周期：queued → running 即开始编排）。
    let recent = v["recent_builds"].as_array().expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["status"], "running");

    // /metrics 同步到队列深度当前值（调度周期 tick 灌入）。
    let text = body_text(bearer_get(&h.app, "/metrics", &pat).await).await;
    assert_eq!(
        metric_value(&text, "sisyphus_queue_depth{reason=\"no_online_agent\"}"),
        Some(1),
        "队列深度当前值经调度周期灌入 /metrics"
    );

    // fake Agent 上线 → 任务下发 → 回执成功 → 构建推进到 succeeded 终态。
    let (mut stream, tx) = connect_fake_agent(h.grpc_addr, &token).await.expect("连接");
    let _first = stream.message().await.expect("recv").expect("msg");
    let specs = collect_job_specs(&mut stream, &tx, 1).await;
    assert_eq!(specs.len(), 1, "阶段 0 一个任务下发");
    for spec in &specs {
        tx.send(ChannelMessage {
            kind: Some(Kind::JobAck(JobAck {
                job_id: spec.job_id.clone(),
                accepted: true,
                error: String::new(),
            })),
        })
        .await
        .expect("ack");
    }
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
        .expect("status");
    }

    // 构建终态 succeeded；概览终态计数 / 警示态 / 最近构建更新。
    let builds = sisyphus_server::store::builds::BuildRepo::new(h.app.state.pool.clone());
    wait_until(|| async {
        builds
            .get(build.id)
            .await
            .expect("查")
            .map(|row| row.status == BuildStatus::Succeeded)
            .unwrap_or(false)
    })
    .await;
    wait_until(|| async {
        let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
        let v = body_json(resp).await;
        v["builds_terminal"]["succeeded"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    let resp = req_with_cookie(&h.app, "GET", "/api/v1/overview", None, Some(&cookie)).await;
    let v = body_json(resp).await;
    assert_eq!(v["queue_depth"].as_u64(), Some(0), "任务终态出队");
    assert_eq!(v["agents_online"].as_u64(), Some(1));
    assert_eq!(v["builds_terminal"]["succeeded"].as_u64(), Some(1));
    assert_eq!(v["builds_terminal"]["failed"].as_u64(), Some(0));
    assert_eq!(v["alerts"]["has_no_match"].as_bool(), Some(false));
    assert_eq!(v["alerts"]["has_offline_agent"].as_bool(), Some(false));
    let recent = v["recent_builds"].as_array().expect("recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["status"], "succeeded");

    // /metrics：事件型终态计数出现（result 标签）+ 时长直方图 + 当前值翻转。
    let text = body_text(bearer_get(&h.app, "/metrics", &pat).await).await;
    assert!(
        text.contains("result=\"succeeded\""),
        "终态计数带 result 标签：\n{text}"
    );
    assert!(
        text.contains("sisyphus_build_duration_seconds_bucket"),
        "构建时长直方图（bucket）出现：\n{text}"
    );
    // 增量断言：成功计数从「不存在（0）」翻到 1（DB 终态迁移唯一命中一次）。
    assert_eq!(
        metric_value(
            &text,
            "sisyphus_builds_terminal_total{result=\"succeeded\"}"
        ),
        Some(1),
        "终态计数器应恰 +1：\n{text}"
    );
    assert_eq!(
        metric_value(&text, "sisyphus_queue_depth{reason=\"no_online_agent\"}"),
        Some(0),
        "出队后队列深度归零"
    );
    assert_eq!(
        metric_value(&text, "sisyphus_agents_online"),
        Some(1),
        "Agent 在线数翻转"
    );

    h.grpc_handle.abort();
}

#[tokio::test]
async fn metrics_public_when_auth_disabled() {
    // `[metrics] auth = false`：/metrics 公开（无凭证 200）。
    let dir = tempfile::tempdir().expect("临时数据目录");
    let pool = store::bootstrap(dir.path()).await.expect("bootstrap");
    let master_key = sisyphus_server::secrets::ensure_master_key(
        &dir.path()
            .join(sisyphus_server::config::MASTER_KEY_FILE_NAME),
    )
    .expect("测试主密钥");
    let state = api::AppState::new(
        pool.clone(),
        dir.path().to_path_buf(),
        false,
        master_key,
        sisyphus_server::config::DEFAULT_POLL_INTERVAL_MINUTES,
        sisyphus_server::config::DEFAULT_RETENTION_DAYS,
        false,
    )
    .await
    .expect("装配 AppState");
    let app = test_app_from_state(state, dir.path());

    let resp = common::get(&app, "/metrics").await;
    assert_eq!(resp.status(), StatusCode::OK, "metrics_auth=false 时公开");
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("")
            .starts_with("text/plain; version=0.0.4"),
        "Prometheus 文本内容类型"
    );

    // 未登录访问 /api/v1/overview 仍 401（metrics 关闭不影响业务鉴权）。
    let overview = common::get(&app, "/api/v1/overview").await;
    assert_eq!(
        overview.status(),
        StatusCode::UNAUTHORIZED,
        "overview 不受 metrics 开关影响，仍需认证"
    );
}

#[tokio::test]
async fn overview_requires_auth() {
    let h = harness().await;
    let resp = common::get(&h.app, "/api/v1/overview").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "未登录 401");
    h.grpc_handle.abort();
}

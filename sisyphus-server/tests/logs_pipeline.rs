//! 日志 server 侧全链路集成测试（票 #73 / B5-T1，ADR-0013）：
//! fake Agent 经真实 tonic 通道发 `LogBatch` 落库（含断线补传重放的幂等）
//! → SSE 端点回放 + 尾随 + Last-Event-ID 续传 + 终态关流 → 整份下载。
//!
//! 形态基准：sched_closed_loop（真实 store + 组合根 + 真实 tonic 通道，
//! 不 spawn Server 进程；SSE 经进程内 Router oneshot 驱动、响应体增量读，
//! Spec B2a 测试缝）。任务终态不经调度循环（本面聚焦日志链路）：直接
//! repo 迁移 + 事件总线广播（与 sched 的 publish_job_status 同形）。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{DEFAULT_PEER, custom_req};
use futures::StreamExt;
use sisyphus_model::pipeline::{Job, Pipeline, Revision, Stage};
use sisyphus_model::validate::BuildSnapshot;
use sisyphus_proto::agent::log_event::Kind as EventKind;
use sisyphus_proto::agent::{
    ChannelMessage, Handshake, LogBatch, LogEvent, OutputChunk, StepEvent, Stream, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_server::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use sisyphus_server::events::Event;
use sisyphus_server::store::agents::NewAgent;
use sisyphus_server::store::builds::{BuildRepo, BuildRow, StartBuild, TriggerSource};
use sisyphus_server::store::jobs::{JobRepo, JobStatus, NewJob};
use sisyphus_server::store::projects::{NewProject, ProjectRepo, ScmType};
use sisyphus_server::store::{LogLocation, LogStore};
use sisyphus_server::{api, grpc, sched, store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

/// 与 Server 同版本（兼容窗口内）。
fn version() -> Version {
    Version {
        major: 1,
        minor: 0,
        patch: 0,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    /// 进程内 Router 组合根（SSE / 下载请求面；含同一 state）。
    app: common::TestApp,
    state: api::AppState,
    grpc_addr: std::net::SocketAddr,
    /// cookie 值（setup admin 会话——全局 admin 视作项目 admin，viewer 档天然满足）。
    cookie: String,
    agent_token: String,
    build: BuildRow,
    job_id: i64,
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

    // REST Router（SSE / 下载消费面；与 gRPC/日志存储共享同一 state）。
    let app = common::test_app_from_state(state.clone(), dir.path());

    // gRPC 服务（LogBatch 落库面；不驱动调度循环——日志帧不经调度）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let grpc_addr = listener.local_addr().expect("addr");
    let sessions = Arc::new(grpc::SessionRegistry::new());
    let grpc_state = state.clone();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(grpc::service(
                grpc_state,
                sessions,
                sched::SchedulerHandle::discard(),
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });

    // 项目 + Agent + 构建 + 任务（任务行 agent_id 指向 Agent——归属校验面）。
    let project = ProjectRepo::new(pool.clone())
        .create(NewProject {
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        })
        .await
        .expect("建项目");
    let agent_token = generate_token(TokenFamily::Agent);
    let code = generate_register_code();
    let agent = state
        .agents
        .create(NewAgent {
            name: "linux-1".into(),
            token_hash: token_hash(&agent_token),
            system_labels: "[]".into(),
            custom_labels: "[]".into(),
            max_concurrency: 1,
            register_code_hash: token_hash(&code),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        })
        .await
        .expect("建 Agent 条目");

    let snapshot = BuildSnapshot::new(
        Pipeline {
            name: "release".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec![],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![],
                }],
            }],
            revision: None,
        },
        Revision {
            number: 1,
            operator: "tester".into(),
            at_ms: 0,
        },
    );
    let build = BuildRepo::new(pool.clone())
        .start(StartBuild {
            project_id: project.id,
            pipeline_name: "release".into(),
            trigger: TriggerSource::Manual,
            trigger_detail: "{}".into(),
            snapshot,
        })
        .await
        .expect("建构建");
    let job = JobRepo::new(pool.clone())
        .insert(NewJob {
            build_id: build.id,
            stage_index: 0,
            name: "compile".into(),
            attempt: 1,
            spec_json: None,
            agent_id: Some(agent.id),
            labels: vec![],
            timeout_minutes: 0,
            retry_count: 0,
            allow_failure: false,
        })
        .await
        .expect("建任务");

    // setup admin + login（SSE/下载端点的 viewer 档——全局 admin 隐含）。
    let cookie = common::setup_and_login(&app).await;

    Harness {
        _dir: dir,
        app,
        state,
        grpc_addr,
        cookie,
        agent_token,
        build,
        job_id: job.id,
    }
}

/// fake Agent 连接（握手 + token 认证），返回上行发送器。
async fn connect_agent(h: &Harness) -> mpsc::Sender<ChannelMessage> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", h.grpc_addr))
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
        MetadataValue::try_from(format!("Bearer {}", h.agent_token)).expect("值"),
    );
    let response = client.connect(request).await.expect("agent connect");
    let mut stream = response.into_inner();
    // 握手回执（会话建立）。
    let msg = stream.message().await.expect("recv").expect("msg");
    assert!(matches!(msg.kind, Some(Kind::Handshake(_))));
    tx
}

/// 单事件 LogBatch 帧（Agent logbuf 的活体形态：每帧一事件）。
fn log_batch(job_id: i64, attempt: i32, event: LogEvent, start_seq: u64) -> ChannelMessage {
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

/// SSE 请求（带认证 cookie；可选附加 header）。
async fn sse_request(
    h: &Harness,
    attempt: i32,
    query: &str,
    headers: &[(&str, String)],
) -> axum::response::Response {
    custom_req(
        &h.app,
        "GET",
        &format!(
            "/api/v1/projects/demo/pipelines/release/builds/{}/jobs/compile/attempts/{}/logs/stream{query}",
            h.build.number, attempt
        ),
        None,
        Some(&h.cookie),
        headers,
        DEFAULT_PEER,
    )
    .await
}

/// 从 SSE 响应体增量读到谓词满足（拼接缓冲），或超时。返回收到的全部
/// 文本与剩余流（尾随面可续读同一流）。
async fn read_sse_until(
    resp: axum::response::Response,
    pred: impl Fn(&str) -> bool,
) -> (String, axum::body::BodyDataStream) {
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
            None => return (buf, stream), // 流结束（关流路径）
        }
    }
}

/// 续读既有 SSE 流到谓词满足或流结束（尾随/关流面；借流可反复续读）。
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
            None => return false, // 流结束（关流验证面）
        }
    }
}

/// 落库事件总数（直查库断言幂等）。
async fn stored_events(h: &Harness) -> usize {
    h.state
        .logs
        .read_from(
            LogLocation {
                build_id: h.build.id,
                job_id: h.job_id,
                attempt: 1,
            },
            0,
        )
        .await
        .expect("读日志")
        .iter()
        .map(|c| sisyphus_server::logs::decode_chunk(c).expect("解码").len())
        .sum()
}

/// 等到谓词成立或超时（异步路径轮询，避免 flaky）。
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

/// 全链路：grpc 落库（含断线补传幂等）→ SSE 回放+尾随+终态关流 →
/// Last-Event-ID 续传 → 整份下载。
#[tokio::test]
async fn log_pipeline_full_chain() {
    let h = harness().await;
    let tx = connect_agent(&h).await;

    // ---- grpc 落库 + 断线补传幂等（AC：重复补传不重不乱序） ----
    tx.send(log_batch(
        h.job_id,
        1,
        output(0, Stream::Stdout, b"preparing\n"),
        0,
    ))
    .await
    .unwrap();
    tx.send(log_batch(
        h.job_id,
        1,
        step_start(1, 0, "cargo build --release"),
        1,
    ))
    .await
    .unwrap();
    tx.send(log_batch(
        h.job_id,
        1,
        output(2, Stream::Stdout, b"   Compiling demo\n"),
        2,
    ))
    .await
    .unwrap();
    // 断线补传（fake Agent 重放文件头——重复 seq 0..=2）+ 新事件 3。
    tx.send(log_batch(
        h.job_id,
        1,
        output(0, Stream::Stdout, b"preparing\n"),
        0,
    ))
    .await
    .unwrap();
    tx.send(log_batch(
        h.job_id,
        1,
        output(2, Stream::Stdout, b"   Compiling demo\n"),
        2,
    ))
    .await
    .unwrap();
    tx.send(log_batch(h.job_id, 1, step_end(3, 0, Some(0)), 3))
        .await
        .unwrap();
    wait_until(|| async { stored_events(&h).await == 4 }).await;
    assert_eq!(stored_events(&h).await, 4, "重放去重后恰 4 事件");

    // ---- SSE 回放（AC：from 起播先补 DB 历史；步骤事件与输出块交织有序） ----
    let resp = sse_request(&h, 1, "", &[]).await;
    assert_eq!(resp.status(), 200);
    let (text, stream) =
        read_sse_until(resp, |t| t.contains("step_end") && t.contains("id: 3")).await;
    // 有序交织：output(0) → step_start(1) → output(2) → step_end(3)，帧带
    // 命名事件 + id 游标 + 前端契约载荷（逐字对齐 sse.ts）。
    let out0 = text.find("event: output\nid: 0").expect("output seq 0");
    let start1 = text
        .find("event: step_start\nid: 1")
        .expect("step_start seq 1");
    let out2 = text.find("event: output\nid: 2").expect("output seq 2");
    let end3 = text.find("event: step_end\nid: 3").expect("step_end seq 3");
    assert!(
        out0 < start1 && start1 < out2 && out2 < end3,
        "到达序交织：{text}"
    );
    assert!(
        text.contains(r#""type":"step_start","seq":1,"step":0,"name":"","command":"cargo build --release","started_at":1000}"#),
        "载荷与前端契约对齐：{text}"
    );
    assert!(
        text.contains(r#""type":"output","seq":0,"stream":"stdout","text":"preparing\n"}"#),
        "输出块载荷：{text}"
    );
    assert!(
        text.contains(r#""type":"step_end","seq":3,"step":0,"exit_code":0,"duration_ms":250}"#),
        "step end 退出码/耗时：{text}"
    );
    // 任务未终态：该流应持续挂着（未关流）——继续在下一节用同一流尾随。

    // ---- 尾随 + 终态关流（AC：实时尾随；终态 flush 后关流） ----
    // （同一流：历史已到 → fake Agent 再发 → 增量到达 → 终态 → job_end + 关流。）
    tx.send(log_batch(
        h.job_id,
        1,
        output(4, Stream::Stderr, b"warning: unused\n"),
        4,
    ))
    .await
    .unwrap();
    let mut buf = text;
    let mut stream = stream;
    assert!(
        continue_sse_until(&mut stream, &mut buf, |t| t.contains("id: 4")).await,
        "实时尾随到达：{buf}"
    );
    assert!(
        buf.contains(r#""stream":"stderr""#) && buf.contains("warning: unused"),
        "尾随事件带 stream 标记：{buf}"
    );

    // 终态关流：任务迁移 succeeded + 广播 JobStatus（sched 同形路径）。
    JobRepo::new(h.state.pool.clone())
        .transition(
            h.job_id,
            JobStatus::Succeeded,
            Some(0),
            None,
            1_700_000_000_000,
        )
        .await
        .expect("任务终态迁移");
    h.state.bus.publish(Event::JobStatus {
        job_id: h.job_id,
        build_id: h.build.id,
        stage_index: 0,
        name: "compile".into(),
        status: JobStatus::Succeeded,
        attempt: 1,
    });
    // job_end 帧送达且 flush 后流结束（continue 到流尽仍未见 job_end 则断言帧）。
    let saw_end = continue_sse_until(&mut stream, &mut buf, |t| t.contains("event: job_end")).await;
    if !saw_end {
        // 流可能已在谓词后立即结束：帧已在 buf 里则通过。
        assert!(buf.contains("event: job_end"), "job_end 必达：{buf}");
    }
    assert!(
        buf.contains(r#""status":"succeeded""#),
        "job_end 载荷：{buf}"
    );

    // 流结束后：再次请求（终态已入库）——回放全部历史 + job_end 后即关流。
    let resp = sse_request(&h, 1, "", &[]).await;
    let (text, mut stream) = read_sse_until(resp, |t| t.contains("event: job_end")).await;
    assert!(text.contains("id: 4"), "终态后回放含全历史：{text}");
    assert!(
        text.contains(r#""type":"job_end""#) && text.contains(r#""status":"succeeded""#),
        "job_end 载荷：{text}"
    );
    let mut text = text;
    let ended = !continue_sse_until(&mut stream, &mut text, |_| false).await;
    assert!(ended, "终态流在 job_end 后关流：{text}");

    // ---- Last-Event-ID 续传（AC：断线重连续传） ----
    let resp = sse_request(&h, 1, "", &[("last-event-id", "1".to_string())]).await;
    let (text, _) = read_sse_until(resp, |t| t.contains("id: 4")).await;
    assert!(
        !text.contains("id: 0"),
        "Last-Event-ID=1 起从 seq 2 续传：{text}"
    );
    assert!(
        text.contains("id: 2") && text.contains("id: 3"),
        "含 2、3：{text}"
    );

    // from query 起播（缺省 0 之外的显式 from）。
    let resp = sse_request(&h, 1, "?from=2", &[]).await;
    let (text, _) = read_sse_until(resp, |t| t.contains("id: 4")).await;
    assert!(
        !text.contains("id: 0") && !text.contains("id: 1"),
        "from=2 起播：{text}"
    );
}

/// 整份下载（AC：text/plain、全部 chunk 解压拼接）。
#[tokio::test]
async fn download_full_log_as_plain_text() {
    let h = harness().await;
    let tx = connect_agent(&h).await;
    tx.send(log_batch(h.job_id, 1, step_start(0, 0, "echo hi"), 0))
        .await
        .unwrap();
    tx.send(log_batch(
        h.job_id,
        1,
        output(1, Stream::Stdout, b"hi\n"),
        1,
    ))
    .await
    .unwrap();
    tx.send(log_batch(h.job_id, 1, step_end(2, 0, Some(0)), 2))
        .await
        .unwrap();
    wait_until(|| async { stored_events(&h).await == 3 }).await;

    let resp = custom_req(
        &h.app,
        "GET",
        &format!(
            "/api/v1/projects/demo/pipelines/release/builds/{}/jobs/compile/attempts/1/logs",
            h.build.number
        ),
        None,
        Some(&h.cookie),
        &[],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    let text = common::body_text(resp).await;
    assert_eq!(text, "$ echo hi\nhi\n", "纯文本渲染：步骤回显 + 输出");
}

/// 无认证 401；不存在的任务/attempt 404；非法 from 422（纪律面）。
#[tokio::test]
async fn stream_endpoint_discipline() {
    let h = harness().await;

    // 未认证：401（全局认证中间件面）。
    let resp = custom_req(
        &h.app,
        "GET",
        &format!(
            "/api/v1/projects/demo/pipelines/release/builds/{}/jobs/compile/attempts/1/logs/stream",
            h.build.number
        ),
        None,
        None,
        &[],
        DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), 401);

    // 任务 attempt 不存在：404（任务行 name+attempt 定位）。
    let resp = sse_request(&h, 9, "", &[]).await;
    assert_eq!(resp.status(), 404);

    // 非法 from：422（不静默放宽）。
    let resp = sse_request(&h, 1, "?from=abc", &[]).await;
    assert_eq!(resp.status(), 422);

    // 非法 Last-Event-ID：422。
    let resp = sse_request(&h, 1, "", &[("last-event-id", "x".to_string())]).await;
    assert_eq!(resp.status(), 422);
}

/// 多事件单帧 chunk（proto 契约允许）：from 游标落在块中间时整块补发、
/// 事件级过滤只发游标及之后的事件（跨游标续传不丢尾）。
#[tokio::test]
async fn multi_event_chunk_straddling_from_cursor() {
    let h = harness().await;
    let tx = connect_agent(&h).await;
    // 一帧三事件（seq 0..=2，start_seq=0）。
    tx.send(ChannelMessage {
        kind: Some(Kind::LogBatch(LogBatch {
            job_id: h.job_id.to_string(),
            attempt: 1,
            start_seq: 0,
            events: vec![
                output(0, Stream::Stdout, b"a\n"),
                output(1, Stream::Stdout, b"b\n"),
                output(2, Stream::Stdout, b"c\n"),
            ],
        })),
    })
    .await
    .unwrap();
    wait_until(|| async { stored_events(&h).await == 3 }).await;

    // from=2：覆盖 seq 2 的 chunk（start_seq=0）须整块补发，只发事件 2。
    let resp = sse_request(&h, 1, "?from=2", &[]).await;
    let (text, _) = read_sse_until(resp, |t| t.contains("id: 2")).await;
    assert!(!text.contains("id: 0"), "事件级过滤：{text}");
    assert!(!text.contains("id: 1"), "事件级过滤：{text}");
    assert!(
        text.contains(r#""text":"c\n""#),
        "游标及之后的事件到达：{text}"
    );
}

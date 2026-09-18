//! 日志 REST 端点（票 #73 / B5-T1，ADR-0013）：SSE 回放+尾随 / 整份下载。
//!
//! - **SSE 端点**（viewer 档）：`GET .../logs/stream?from=<seq>`（缺省 0）。
//!   先从 DB/Agent 热历史补历史、再接事件总线和 Agent 实时尾随；浏览器原生 EventSource 断线
//!   自动重连带 `Last-Event-ID`（即 seq）原地续传——header 优先于 `from`
//!   query（重连 URL 仍携原始 from，header 才是游标真相）。流元素带类型
//!   （输出块带 stream 标记 + 步骤生命周期事件），SSE 命名事件（`event:
//!   <type>`）+ `id: <seq>` 承载续传游标，契约与前端 `sse.ts` 逐字对齐。
//!   任务终态事件（job_end，自 jobs 行状态合成——proto 日志流不含终态）
//!   送达并 flush 后关流。旧 SQLite 广播可丢时从 DB 游标重放；新 Agent 直播
//!   按 seq 请求本地回放，DB 不持续保存直播正文。
//! - **整份下载**（viewer 档）：同资源 `GET .../logs`（text/plain；全部
//!   chunk 解压拼接为纯文本渲染：输出原样含 ANSI、步骤回显 `$ <命令>`）。
//!
//! 定位：行按 (build, job, attempt, seq) 定位（ADR-0013）；路径解析
//! project/pipeline/number → 构建行、job 名/attempt → 任务行（重跑同任务
//! 占新行，name+attempt 唯一定位）。

use std::collections::VecDeque;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response, sse::Event as SseEvent};
use serde::Deserialize;
use utoipa::IntoParams;

use super::AppState;
use super::builds::load_build;
use super::error::{ApiError, ErrorBody, ValidationIssue};
use super::policy::RequireViewer;
use crate::api::artifacts::AgentAuth;
use crate::config::LogArchiveBackend;
use crate::events::Event;
use crate::grpc::log_events_from_proto;
use crate::logs::{self, JobEndEvent, LogStreamEvent};
use crate::store::ArchiveIndex;
use crate::store::LogLocation;
use crate::store::LogStore;
use crate::store::builds::BuildRow;
use crate::store::jobs::{JobRepo, JobRow};
use futures::StreamExt;
use sisyphus_proto::agent::LogBatch;
use tokio::io::AsyncWriteExt;

/// SSE 尾随状态机（`futures::stream::unfold` 的折叠态）。
struct LogTail {
    /// 组合根状态（DB 读 + 事件总线已订阅的接收器在下）。
    state: AppState,
    /// 日志定位。
    loc: LogLocation,
    /// 事件总线接收器（订阅先于历史读取——历史读完后无漏窗）。
    rx: tokio::sync::broadcast::Receiver<Event>,
    /// Agent 按需直播上游；Server 只在至少一名观看者时创建/复用它。
    live_rx: tokio::sync::broadcast::Receiver<LogBatch>,
    /// Server 小型热历史窗口，在浏览器重连时不必重新建立第二条上游订阅。
    live_queue: VecDeque<LogBatch>,
    agent_id: Option<i64>,
    attempt: i32,
    /// Agent 当前离线时先发一次明确的不可用事件；重连后的自动订阅仍可
    /// 继续把日志送到同一 SSE 流。
    unavailable_pending: bool,
    _subscription: Option<LogSubscriptionGuard>,
    /// 下一待发 seq（from / Last-Event-ID+1 起，随发随推进）。
    cursor: u64,
    /// 已解码待发的事件队列（DB 一批读出逐条发）。
    queue: VecDeque<SseEvent>,
    /// job_end 已合成：队列发完即关流。
    done: bool,
    /// 需要从 DB 重读（首轮回放 / LogAppended / Lagged 自愈）。
    reread: bool,
    /// 需要复核任务终态（首轮 / JobStatus 事件）。
    check_terminal: bool,
}

/// SSE 状态机销毁时退订 Agent；正常终态与浏览器主动关闭都会触发 Drop。
struct LogSubscriptionGuard {
    sessions: std::sync::Arc<crate::grpc::SessionRegistry>,
    agent_id: i64,
    job_id: String,
    attempt: i32,
}

impl Drop for LogSubscriptionGuard {
    fn drop(&mut self) {
        let sessions = self.sessions.clone();
        let agent_id = self.agent_id;
        let job_id = self.job_id.clone();
        let attempt = self.attempt;
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                sessions.unsubscribe_log(agent_id, &job_id, attempt).await;
            });
        }
    }
}

impl LogTail {
    /// 把 Agent 直播帧按 SSE 游标入队；空帧是 Server 内部的 Agent 离线哨兵。
    fn enqueue_live_batch(&mut self, batch: LogBatch) -> Option<u64> {
        if batch.events.is_empty() {
            self.unavailable_pending = true;
            return None;
        }
        // Agent 的非阻塞上行可能丢掉邮箱已满时的热帧。先验证原始 seq
        // 连续性；发现缺口时丢弃当前帧并让调用方从游标请求一次本地回放，
        // 避免把后续帧提前送给浏览器而永久跳过缺失内容。
        for event in batch.events {
            if event.seq < self.cursor {
                continue;
            }
            if event.seq > self.cursor {
                return Some(self.cursor);
            }
            let single = LogBatch {
                job_id: batch.job_id.clone(),
                attempt: batch.attempt,
                start_seq: event.seq,
                events: vec![event.clone()],
            };
            for event in log_events_from_proto(&single) {
                let seq = event.seq();
                self.queue.push_back(sse_event(&event));
                self.cursor = seq.saturating_add(1);
            }
            // 未知的未来事件类型也占用 seq；游标必须前移，否则每次重连都会
            // 对同一未知帧反复请求回放。
            self.cursor = self.cursor.max(event.seq.saturating_add(1));
        }
        None
    }

    /// 从 DB 自游标重读并解码入队；返回新读到的条数。读失败向流报告错误，
    /// 让 EventSource 重连，而非将失败误判成终态归档已读完。损坏 chunk 跳过。
    async fn drain_db(&mut self) -> Result<usize, crate::store::StoreError> {
        let chunks = self.state.logs.read_from(self.loc, self.cursor).await?;
        let mut n = 0;
        for chunk in chunks {
            match logs::decode_chunk(&chunk) {
                Ok(events) => {
                    for ev in events {
                        // 跨游标 chunk（多事件块覆盖 from）：只发游标及之后的事件。
                        if ev.seq() < self.cursor {
                            continue;
                        }
                        let seq = ev.seq();
                        self.queue.push_back(sse_event(&ev));
                        self.cursor = seq + 1;
                        n += 1;
                    }
                }
                Err(e) => {
                    // 落库内容出自本 codec：损坏即库异常，记日志跳过不炸流。
                    tracing::warn!(job_id = self.loc.job_id, error = %e, "日志 chunk 解码失败，跳过");
                }
            }
        }
        Ok(n)
    }

    /// 复核任务终态：终态即合成 job_end 入队、置 done（队列发完关流）。
    async fn check_terminal(&mut self) {
        let job = match JobRepo::new(self.state.pool.clone())
            .get(self.loc.job_id)
            .await
        {
            Ok(Some(job)) => job,
            _ => return, // 行不存在/查库失败：不合成终态（下一轮再试）
        };
        if job.status.is_terminal() {
            let end = JobEndEvent::new(self.cursor, job.status.as_str(), job.exit_code);
            self.queue.push_back(sse_job_end(&end));
            self.done = true;
        }
    }
}

/// 日志流查询参数。
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct LogStreamQuery {
    /// 起播 seq（缺省 0；Last-Event-ID header 优先——重连续传游标）。
    pub from: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ArchiveGrantRequest {
    index: ArchiveIndex,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct ArchiveGrantResponse {
    backend: &'static str,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

pub(crate) async fn agent_archive_grant(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, attempt)): Path<(i64, i32)>,
    Json(request): Json<ArchiveGrantRequest>,
) -> Result<Json<ArchiveGrantResponse>, ApiError> {
    crate::store::jobs::JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
        .filter(|job| job.agent_id == Some(agent.agent_id) && job.attempt == attempt)
        .ok_or_else(|| ApiError::resource_not_found(format!("任务 {job_id} 不存在")))?;
    if state
        .log_archives
        .status(job_id, attempt)
        .await?
        .is_some_and(|archive| archive.state == "lost")
    {
        return Ok(Json(ArchiveGrantResponse {
            backend: "local",
            state: "lost",
            url: None,
        }));
    }
    state
        .log_archives
        .record_execution_end(job_id, attempt, request.index.execution_finished_at_ms)
        .await?;
    if state.log_archive_backend == LogArchiveBackend::Local {
        return Ok(Json(ArchiveGrantResponse {
            backend: "local",
            state: "pending",
            url: None,
        }));
    }
    let s3 = state
        .s3
        .as_ref()
        .ok_or_else(|| ApiError::conflict("S3 日志后端未配置"))?;
    let key = state
        .log_archives
        .grant_s3(job_id, attempt, s3.prefix(), &request.index)
        .await
        .map_err(archive_store_error)?;
    let url = key
        .map(|key| s3.presign_put(&key, 300))
        .transpose()
        .map_err(|e| ApiError::internal("签发日志归档直传 URL", &e))?;
    Ok(Json(ArchiveGrantResponse {
        backend: "s3",
        state: if url.is_some() { "pending" } else { "ready" },
        url,
    }))
}

pub(crate) async fn agent_archive_complete(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, attempt)): Path<(i64, i32)>,
) -> Result<Json<ArchiveUploadResponse>, ApiError> {
    crate::store::jobs::JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
        .filter(|job| job.agent_id == Some(agent.agent_id) && job.attempt == attempt)
        .ok_or_else(|| ApiError::resource_not_found(format!("任务 {job_id} 不存在")))?;
    state
        .log_archives
        .publish_s3(
            job_id,
            attempt,
            state.artifact_transfer_limits.copy_object_limit,
            state.artifact_transfer_limits.copy_part_size,
        )
        .await
        .map_err(archive_store_error)?;
    Ok(Json(ArchiveUploadResponse {
        job_id,
        attempt,
        state: "ready".into(),
    }))
}

fn archive_store_error(error: crate::store::StoreError) -> ApiError {
    match error {
        crate::store::StoreError::Conflict(message) => ApiError::conflict(message),
        crate::store::StoreError::NotFound(message) => ApiError::resource_not_found(message),
        crate::store::StoreError::Invalid(message) => ApiError::validation(
            "日志归档参数非法",
            vec![ValidationIssue {
                path: "archive".into(),
                message,
            }],
        ),
        other => ApiError::internal("登记日志归档", &other),
    }
}

/// Agent 终态归档上传：正文流式落临时文件，Server 校验大小/SHA-256 后原子登记 ready。
/// 索引通过 JSON header 传递，避免把归档正文拼入内存或 JSON。
pub(crate) async fn agent_archive_upload(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, attempt)): Path<(i64, i32)>,
    Query(query): Query<ArchiveUploadQuery>,
    request: Request,
) -> Result<Json<ArchiveUploadResponse>, ApiError> {
    if state.log_archive_backend != LogArchiveBackend::Local {
        return Err(ApiError::conflict("当前日志归档后端为 S3，请使用直传许可"));
    }
    crate::store::jobs::JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
        .filter(|job| job.agent_id == Some(agent.agent_id) && job.attempt == attempt)
        .ok_or_else(|| ApiError::resource_not_found(format!("任务 {job_id} 不存在")))?;
    state
        .log_archives
        .mark_pending(job_id, attempt)
        .await
        .map_err(|e| ApiError::internal("登记待归档日志", &e))?;
    let index_raw = request
        .headers()
        .get("x-sisyphus-archive-index")
        .ok_or_else(|| {
            ApiError::validation(
                "日志归档索引缺失",
                vec![ValidationIssue {
                    path: "x-sisyphus-archive-index".into(),
                    message: "必须提供归档索引".into(),
                }],
            )
        })?
        .to_str()
        .map_err(|_| ApiError::validation("日志归档索引非法", vec![]))?;
    let index: ArchiveIndex = serde_json::from_str(index_raw).map_err(|e| {
        ApiError::validation(
            "日志归档索引非法",
            vec![ValidationIssue {
                path: "index".into(),
                message: e.to_string(),
            }],
        )
    })?;
    let root = state.log_archives.root().join("tmp");
    state
        .log_archives
        .record_execution_end(job_id, attempt, index.execution_finished_at_ms)
        .await?;
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| ApiError::internal("创建日志归档临时目录", &e))?;
    let temp = root.join(format!(
        "{job_id}-{attempt}-{}.upload",
        crate::store::now_ms()
    ));
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|e| ApiError::internal("创建日志归档临时文件", &e))?;
    let mut stream = request.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::internal("接收日志归档", &e))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| ApiError::internal("写入日志归档", &e))?;
    }
    file.sync_all()
        .await
        .map_err(|e| ApiError::internal("同步日志归档", &e))?;
    if let Err(error) = state
        .log_archives
        .publish(job_id, attempt, &temp, query.size, &query.sha256, &index)
        .await
    {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(match error {
            crate::store::StoreError::Conflict(message) => ApiError::conflict(message),
            crate::store::StoreError::Invalid(message) => ApiError::validation(
                "日志归档参数非法",
                vec![ValidationIssue {
                    path: "archive".into(),
                    message,
                }],
            ),
            other => ApiError::internal("登记日志归档", &other),
        });
    }
    Ok(Json(ArchiveUploadResponse {
        job_id,
        attempt,
        state: "ready".into(),
    }))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct ArchiveUploadQuery {
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ArchiveUploadResponse {
    pub job_id: i64,
    pub attempt: i32,
    pub state: String,
}

/// Agent 离线且尚未有可读归档时的明确 SSE 状态事件。
#[derive(Debug, serde::Serialize)]
struct LogUnavailableEvent {
    #[serde(rename = "type")]
    kind: String,
    reason: String,
}

/// SSE 日志流端点（viewer 档，ADR-0013）：`from` 起播先补 DB 历史、再接
/// 事件总线尾随；`Last-Event-ID`（原生 EventSource 断线重连自动携带）即
/// seq 游标，续传自 id+1 起。任务终态送达并 flush 后关流。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/jobs/{job}/attempts/{attempt}/logs/stream",
    tag = "builds",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
        ("job" = String, Path, description = "任务名"),
        ("attempt" = i32, Path, description = "第几次执行（重跑同任务占新行）"),
        LogStreamQuery,
    ),
    responses(
        (status = 200, description = "SSE 日志流（text/event-stream；命名事件 output/step_start/step_end/truncated/job_end，id=seq）", content_type = "text/event-stream"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（查看日志需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号/任务/attempt 不存在", body = ErrorBody),
        (status = 422, description = "from/Last-Event-ID 非法（须为非负整数）", body = ErrorBody),
    )
)]
pub async fn stream(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number, job_name, attempt)): Path<(String, String, i64, String, i32)>,
    Query(query): Query<LogStreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let job = load_job(&state, &build, &job_name, attempt).await?;

    // 起播游标：Last-Event-ID（最后送达事件 id）+1 优先；否则 from query
    //（缺省 0，含端起播）。两者皆须非负整数，非法 422（不静默放宽）。
    let from = match header_last_event_id(&headers)? {
        Some(last) => last.saturating_add(1),
        None => parse_seq(query.from.as_deref(), "from")?,
    };

    if let Some(archive) = state.log_archives.status(job.id, attempt).await?
        && archive.state == "lost"
    {
        let event = LogUnavailableEvent {
            kind: "log_unavailable".into(),
            reason: archive
                .lost_reason
                .unwrap_or_else(|| "permanently_lost".into()),
        };
        return Ok(Sse::new(futures::stream::iter([Ok::<_, std::io::Error>(
            sse_unavailable(&event),
        )]))
        .into_response());
    }

    // 订阅 Agent 先于历史读取（窗口无漏）：首名观看者触发一次上游订阅，
    // 后续浏览器复用同一 broadcast；Agent 离线时保留观看者状态供重连恢复。
    let rx = state.bus.subscribe();
    let (live_rx, online, initial, subscription) = if let Some(agent_id) = job.agent_id {
        let (live_rx, online, initial) = state
            .agent_sessions
            .subscribe_log(agent_id, &job.id.to_string(), attempt, from)
            .await;
        (
            live_rx,
            online,
            initial,
            Some(LogSubscriptionGuard {
                sessions: state.agent_sessions.clone(),
                agent_id,
                job_id: job.id.to_string(),
                attempt,
            }),
        )
    } else {
        let (_, live_rx) = tokio::sync::broadcast::channel(1);
        (live_rx, false, Vec::new(), None)
    };
    let tail = LogTail {
        state: state.clone(),
        loc: logs::location(build.id, job.id, attempt),
        rx,
        live_rx,
        live_queue: initial.into(),
        agent_id: job.agent_id,
        attempt,
        unavailable_pending: job.agent_id.is_some() && !online && !job.status.is_terminal(),
        _subscription: subscription,
        cursor: from,
        queue: VecDeque::new(),
        done: false,
        reread: true,
        check_terminal: true,
    };

    let stream = futures::stream::unfold(tail, |mut tail| async move {
        loop {
            if let Some(ev) = tail.queue.pop_front() {
                return Some((Ok::<_, std::io::Error>(ev), tail));
            }
            if tail.done {
                return None; // job_end 已发：关流（flush 后）
            }
            if tail.reread {
                tail.reread = false;
                let read_count = match tail.drain_db().await {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::warn!(job_id = tail.loc.job_id, error = %error, "日志回放读取失败，关闭流以便客户端重试");
                        tail.done = true;
                        return Some((Err(std::io::Error::other(error.to_string())), tail));
                    }
                };
                if read_count > 0 {
                    // 归档和 SQLite 都按有限批次返回；队列排空后继续从
                    // 新游标读取下一帧，避免历史归档一次性驻留内存。
                    tail.reread = true;
                    continue; // 历史有货：先发
                }
            }
            if let Some(batch) = tail.live_queue.pop_front() {
                if let Some(from_seq) = tail.enqueue_live_batch(batch)
                    && let Some(agent_id) = tail.agent_id
                {
                    tail.state
                        .agent_sessions
                        .request_log_replay(
                            agent_id,
                            &tail.loc.job_id.to_string(),
                            tail.attempt,
                            from_seq,
                        )
                        .await;
                }
                continue;
            }
            if tail.check_terminal {
                tail.check_terminal = false;
                tail.check_terminal().await;
                if !tail.queue.is_empty() {
                    continue; // job_end 入队：发完关流
                }
            }
            if tail.unavailable_pending {
                tail.unavailable_pending = false;
                let event = LogUnavailableEvent {
                    kind: "log_unavailable".into(),
                    reason: "agent_offline".into(),
                };
                tail.queue.push_back(sse_unavailable(&event));
                continue;
            }
            // 两条热路径并行等待：任务状态由事件总线驱动，正文由 Agent
            // broadcast 驱动。任一路丢帧都按 seq 请求 Agent 回放。
            tokio::select! {
                live = tail.live_rx.recv() => match live {
                    Ok(batch) => {
                        if let Some(from_seq) = tail.enqueue_live_batch(batch)
                            && let Some(agent_id) = tail.agent_id
                        {
                            tail.state.agent_sessions.request_log_replay(
                                agent_id,
                                &tail.loc.job_id.to_string(),
                                tail.attempt,
                                from_seq,
                            ).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(agent_id) = tail.agent_id {
                            tail.state.agent_sessions.request_log_replay(
                                agent_id,
                                &tail.loc.job_id.to_string(),
                                tail.attempt,
                                tail.cursor,
                            ).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                },
                event = tail.rx.recv() => match event {
                    Ok(Event::LogAppended { job_id, .. }) if job_id == tail.loc.job_id => {
                        // 旧 Agent 兼容面仍把 LogBatch 写入 SQLite；新 Agent
                        // 直播正文走 live_rx，不产生该事件。
                        tail.reread = true;
                    }
                    Ok(Event::JobStatus { job_id, .. }) if job_id == tail.loc.job_id => {
                        tail.reread = true;
                        tail.check_terminal = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tail.check_terminal = true;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    _ => {}
                }
            }
        }
    });

    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}

/// 整份日志下载端点（viewer 档，ADR-0013）：text/plain，全部 chunk 解压
/// 拼接为纯文本（输出原样含 ANSI、步骤回显、截断标注）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/jobs/{job}/attempts/{attempt}/logs",
    tag = "builds",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
        ("job" = String, Path, description = "任务名"),
        ("attempt" = i32, Path, description = "第几次执行"),
    ),
    responses(
        (status = 200, description = "整份日志纯文本（text/plain；输出原样含 ANSI、步骤回显 $ 命令）", content_type = "text/plain"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（下载日志需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号/任务/attempt 不存在", body = ErrorBody),
    )
)]
pub async fn download(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number, job_name, attempt)): Path<(String, String, i64, String, i32)>,
) -> Result<Response, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let job = load_job(&state, &build, &job_name, attempt).await?;
    let loc = logs::location(build.id, job.id, attempt);
    if let Some(archive) = state.log_archives.status(job.id, attempt).await?
        && archive.state == "lost"
    {
        return Err(ApiError::conflict(format!(
            "日志不可用：{}",
            archive
                .lost_reason
                .unwrap_or_else(|| "permanently_lost".into())
        )));
    }
    if let Some(stream) = state
        .log_archives
        .stream_plain(job.id, attempt)
        .await
        .map_err(|e| ApiError::internal("日志归档读取", &e))?
    {
        let body = Body::from_stream(stream.map(|result| result.map(axum::body::Bytes::from)));
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body,
        )
            .into_response());
    }
    let chunks = state
        .logs
        .read_from(loc, 0)
        .await
        .map_err(|e| ApiError::internal("日志读取", &e))?;
    let mut events = Vec::new();
    for chunk in chunks {
        match logs::decode_chunk(&chunk) {
            Ok(mut evs) => events.append(&mut evs),
            Err(e) => tracing::warn!(job_id = job.id, error = %e, "日志 chunk 解码失败，跳过"),
        }
    }
    let text = logs::render_plain(&events);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// 按构建行 + 任务名 + attempt 定位任务行（重跑同任务占新行 attempt+1，
/// name+attempt 唯一）；不存在 404。
pub(super) async fn load_job(
    state: &AppState,
    build: &BuildRow,
    job_name: &str,
    attempt: i32,
) -> Result<JobRow, ApiError> {
    let jobs = JobRepo::new(state.pool.clone())
        .list_by_build(build.id)
        .await?;
    jobs.into_iter()
        .find(|j| j.name == job_name && j.attempt == attempt)
        .ok_or_else(|| {
            ApiError::resource_not_found(format!("任务 {job_name}（attempt {attempt}）不存在"))
        })
}

/// 解析非负整数 seq 参数（`from`）；非法 422。
fn parse_seq(raw: Option<&str>, field: &str) -> Result<u64, ApiError> {
    match raw {
        None => Ok(0),
        Some(s) => s.parse::<u64>().map_err(|_| {
            ApiError::validation(
                "日志流参数非法",
                vec![ValidationIssue {
                    path: field.into(),
                    message: format!("{field} 须为非负整数，收到：{s}"),
                }],
            )
        }),
    }
}

/// 取 `Last-Event-ID` header（原生 EventSource 断线重连自动携带，即最后
/// 送达事件的 seq）。缺失返回 None；非法 422。
fn header_last_event_id(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    let Some(value) = headers.get("last-event-id") else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| invalid_last_event_id("<非文本>"))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    raw.trim()
        .parse::<u64>()
        .map(Some)
        .map_err(|_| invalid_last_event_id(raw))
}

/// Last-Event-ID 非法的 422。
fn invalid_last_event_id(raw: &str) -> ApiError {
    ApiError::validation(
        "日志流参数非法",
        vec![ValidationIssue {
            path: "Last-Event-ID".into(),
            message: format!("Last-Event-ID 须为非负整数，收到：{raw}"),
        }],
    )
}

/// 流事件 → SSE 帧：命名事件（`event: <type>`，与前端 `sse.ts` 的
/// EVENT_TYPES 对齐）+ `id: <seq>`（续传游标）+ JSON 载荷（serde 形态即
/// 前端解析契约）。
fn sse_event(ev: &LogStreamEvent) -> SseEvent {
    let name = match ev {
        LogStreamEvent::Output { .. } => "output",
        LogStreamEvent::StepStart { .. } => "step_start",
        LogStreamEvent::StepEnd { .. } => "step_end",
        LogStreamEvent::Truncated { .. } => "truncated",
    };
    SseEvent::default()
        .event(name)
        .id(ev.seq().to_string())
        .json_data(ev)
        .expect("日志事件 JSON 恒可序列化")
}

/// job_end 合成事件 → SSE 帧。
fn sse_job_end(end: &JobEndEvent) -> SseEvent {
    SseEvent::default()
        .event("job_end")
        .id(end.seq.to_string())
        .json_data(end)
        .expect("job_end JSON 恒可序列化")
}

fn sse_unavailable(event: &LogUnavailableEvent) -> SseEvent {
    SseEvent::default()
        .event("log_unavailable")
        .json_data(event)
        .expect("日志不可用事件 JSON 恒可序列化")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seq_defaults_zero_and_rejects_garbage() {
        assert_eq!(parse_seq(None, "from").expect("缺省"), 0);
        assert_eq!(parse_seq(Some("7"), "from").expect("数字"), 7);
        let err = parse_seq(Some("x"), "from").unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(parse_seq(Some("-1"), "from").is_err(), "负数拒绝");
    }

    #[test]
    fn header_last_event_id_parses_or_rejects() {
        let mut headers = HeaderMap::new();
        assert_eq!(header_last_event_id(&headers).unwrap(), None);

        headers.insert("last-event-id", "5".parse().unwrap());
        assert_eq!(header_last_event_id(&headers).unwrap(), Some(5));

        headers.insert("last-event-id", "abc".parse().unwrap());
        assert_eq!(
            header_last_event_id(&headers).unwrap_err().status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // 空值视为缺失（浏览器初连不发该 header；个别代理发空串）。
        headers.insert("last-event-id", "".parse().unwrap());
        assert_eq!(header_last_event_id(&headers).unwrap(), None);
    }

    #[test]
    fn sse_event_carries_name_id_and_payload() {
        let ev = LogStreamEvent::Output {
            seq: 3,
            stream: crate::logs::LogStream::Stderr,
            text: "boom".into(),
        };
        let frame = sse_event(&ev);
        // Debug 呈预渲染帧（内部引号转义），断言三个要素俱在。
        let text = format!("{frame:?}");
        assert!(text.contains("event: output"), "{text}");
        assert!(text.contains("id: 3"), "{text}");
        assert!(text.contains("stream\\\":\\\"stderr"), "{text}");
    }
}

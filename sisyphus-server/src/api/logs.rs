//! 日志 REST 端点（票 #73 / B5-T1，ADR-0013）：SSE 回放+尾随 / 整份下载。
//!
//! - **SSE 端点**（viewer 档）：`GET .../logs/stream?from=<seq>`（缺省 0）。
//!   先从 DB 补历史、再接事件总线实时尾随；浏览器原生 EventSource 断线
//!   自动重连带 `Last-Event-ID`（即 seq）原地续传——header 优先于 `from`
//!   query（重连 URL 仍携原始 from，header 才是游标真相）。流元素带类型
//!   （输出块带 stream 标记 + 步骤生命周期事件），SSE 命名事件（`event:
//!   <type>`）+ `id: <seq>` 承载续传游标，契约与前端 `sse.ts` 逐字对齐。
//!   任务终态事件（job_end，自 jobs 行状态合成——proto 日志流不含终态）
//!   送达并 flush 后关流。广播可丢：尾随收到 [`Event::LogAppended`] 后从
//!   DB 游标重放（Lagged 亦自愈），DB 是真相源（ADR-0005 重放兜底）。
//! - **整份下载**（viewer 档）：同资源 `GET .../logs`（text/plain；全部
//!   chunk 解压拼接为纯文本渲染：输出原样含 ANSI、步骤回显 `$ <命令>`）。
//!
//! 定位：行按 (build, job, attempt, seq) 定位（ADR-0013）；路径解析
//! project/pipeline/number → 构建行、job 名/attempt → 任务行（重跑同任务
//! 占新行，name+attempt 唯一定位）。

use std::collections::VecDeque;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response, sse::Event as SseEvent};
use serde::Deserialize;
use utoipa::IntoParams;

use super::AppState;
use super::builds::load_build;
use super::error::{ApiError, ErrorBody, ValidationIssue};
use super::policy::RequireViewer;
use crate::events::Event;
use crate::logs::{self, JobEndEvent, LogStreamEvent};
use crate::store::LogLocation;
use crate::store::LogStore;
use crate::store::builds::BuildRow;
use crate::store::jobs::{JobRepo, JobRow};

/// SSE 尾随状态机（`futures::stream::unfold` 的折叠态）。
struct LogTail {
    /// 组合根状态（DB 读 + 事件总线已订阅的接收器在下）。
    state: AppState,
    /// 日志定位。
    loc: LogLocation,
    /// 事件总线接收器（订阅先于历史读取——历史读完后无漏窗）。
    rx: tokio::sync::broadcast::Receiver<Event>,
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

impl LogTail {
    /// 从 DB 自游标重读并解码入队；返回新读到的条数。读失败记日志按空
    /// 处理（下一轮总线事件再试——不炸流）。损坏 chunk 跳过（解码层）。
    async fn drain_db(&mut self) -> usize {
        let chunks = match self.state.logs.read_from(self.loc, self.cursor).await {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!(job_id = self.loc.job_id, error = %e, "日志回放读库失败");
                return 0;
            }
        };
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
        n
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

    // 订阅先于历史读取（窗口无漏）：订阅后发生的迁移必经总线（或 Lagged
    // 自愈重读）；订阅前的终态由首轮 check_terminal 从行状态捕获。
    let rx = state.bus.subscribe();
    let tail = LogTail {
        state: state.clone(),
        loc: logs::location(build.id, job.id, attempt),
        rx,
        cursor: from,
        queue: VecDeque::new(),
        done: false,
        reread: true,
        check_terminal: true,
    };

    let stream = futures::stream::unfold(tail, |mut tail| async move {
        loop {
            if let Some(ev) = tail.queue.pop_front() {
                return Some((Ok::<_, std::convert::Infallible>(ev), tail));
            }
            if tail.done {
                return None; // job_end 已发：关流（flush 后）
            }
            if tail.reread {
                tail.reread = false;
                if tail.drain_db().await > 0 {
                    continue; // 历史有货：先发
                }
            }
            if tail.check_terminal {
                tail.check_terminal = false;
                tail.check_terminal().await;
                if !tail.queue.is_empty() {
                    continue; // job_end 入队：发完关流
                }
            }
            // 无新事件：尾随等总线（可丢热通知——丢了靠下一轮 DB 重读）。
            match tail.rx.recv().await {
                Ok(Event::LogAppended { job_id, .. }) if job_id == tail.loc.job_id => {
                    tail.reread = true;
                }
                Ok(Event::JobStatus { job_id, .. }) if job_id == tail.loc.job_id => {
                    tail.reread = true;
                    tail.check_terminal = true;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 慢消费丢中间消息：从游标重读 DB 自愈（真相源兜底）。
                    tail.reread = true;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return None; // 总线关闭（进程关闭）：关流
                }
                Ok(_) => {} // 无关事件（其它任务/构建/Agent 面）：继续等
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
async fn load_job(
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

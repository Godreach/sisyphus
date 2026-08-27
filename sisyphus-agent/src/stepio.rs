//! 步骤 IO 共享：输出流式编码 + 步骤生命周期事件 + per-job 日志截断
//! （ADR-0013；从 runner 抽出，票 B3-T6 / #60）。
//!
//! shell 步骤（[`crate::runner`]）与 checkout 步骤（[`crate::checkout`]）共用
//! 同一套编码面：进程 stdout/stderr 经脱敏（[`crate::redact::Redactor`]）+
//! 截断（[`Truncation`]）编码为 `OutputChunk`/`Truncated`，步骤 start/end 编码
//! 为 `StepEvent`，统一经 [`crate::logbuf::LogBuffer`] 编号 seq 落盘转发。
//! [`run_streamed_step`] 把「起进程 → 后台写 stdin（可选）+ 流式编码 stdout/stderr
//! → wait（取消/超时竞争）→ join 流任务」封为一步，两类步骤的每条子命令复用。
//!
//! - **per-job 截断**（ADR-0013）：[`Truncation`] 由调用方（runner `run_steps`）
//!   per-job 创建一次、跨步骤 + 跨子命令共享同一计数——多步任务的总输出不越限。
//! - **脱敏跨输出块**：[`crate::redact::Redactor`] per-stream 有状态，跨块边界吸收
//!   机密前缀；checkout 凭据 password 已在 runner 的 `secret_values` 集合内
//!   （`collect_secrets`），与机密 env 同道脱敏。
//! - **stdin**（svn `--password-from-stdin`）：[`run_streamed_step`] 的 `stdin`
//!   参数 `Some(data)` 时后台写 + 关闭（写失败忽略——进程可能早退报错）。

use std::sync::Arc;
use std::time::Duration;

use sisyphus_proto::agent::{
    LogEvent, OutputChunk, StepEvent, Stream, Truncated, log_event::Kind as EventKind,
};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::exec::{SpawnedStep, StepOutcome};
use crate::logbuf::LogBuffer;
use crate::redact::Redactor;

/// 单次 stdout/stderr 读取缓冲（16KiB；流式背压与 seq 粒度的平衡）。
const READ_BUF: usize = 16 * 1024;

// ============================================================
// per-job 日志截断计数（跨步骤 + 跨子命令共享）
// ============================================================

/// per-job 日志字节截断计数：累计已发字节，超 `limit` 后丢弃并插入一次
/// `Truncated` 标记（不判败，ADR-0013）。stdout/stderr 与多步/多子命令共享同一计数。
/// 由调用方 per-job 创建（`Arc` 克隆分发给各流/各子命令）。
pub struct Truncation {
    inner: std::sync::Mutex<TruncationState>,
}

struct TruncationState {
    limit: u64,
    emitted: u64,
    truncated: bool,
}

impl Truncation {
    /// 以 per-job 字节上限构造。
    pub fn new(limit: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(TruncationState {
                limit,
                emitted: 0,
                truncated: false,
            }),
        }
    }

    /// 申请发出 `len` 字节。返回 `(允许发出的字节数, 是否插入截断标记)`。
    /// 已截断后再次调用 → `(0, false)`（整块丢弃、不再标记）。
    pub fn acquire(&self, len: usize) -> (usize, bool) {
        let mut s = self.inner.lock().expect("trunc 锁");
        if s.truncated {
            return (0, false);
        }
        let len64 = len as u64;
        if s.emitted.saturating_add(len64) <= s.limit {
            s.emitted = s.emitted.saturating_add(len64);
            return (len, false);
        }
        // 越限：发剩余配额（可能为 0），置截断标记。
        let fits = s.limit.saturating_sub(s.emitted);
        s.emitted = s.limit;
        s.truncated = true;
        (fits as usize, true)
    }
}

// ============================================================
// 步骤生命周期事件（start: 命令回显 / end: 退出码）
// ============================================================

/// 构造一个步骤生命周期事件。start：`exit_code=None`、`ended_at=0`、`command`
/// 携命令回显；end：`exit_code=Some`、`command` 空（回显只在 start 携带，
/// ADR-0013）。纯函数，消除调用点重复构造。
pub fn step_event(
    seq: i32,
    started: i64,
    ended: i64,
    exit_code: Option<i32>,
    command: &str,
) -> StepEvent {
    StepEvent {
        seq,
        step_started_at_ms: started,
        step_ended_at_ms: ended,
        exit_code,
        command: command.to_string(),
    }
}

/// 编码一个步骤生命周期事件（start: exit_code=None / end: exit_code=Some）。
/// 调用方经 [`step_event`] 构造 [`StepEvent`]。seq 由 [`LogBuffer`] 编号。
pub async fn emit_step(logbuf: &LogBuffer, job_id: &str, attempt: i32, event: StepEvent) {
    let _ = logbuf
        .append(
            job_id,
            attempt,
            LogEvent {
                seq: 0,
                kind: Some(EventKind::Step(event)),
            },
        )
        .await;
}

// ============================================================
// 输出流式编码（脱敏 + 截断 → OutputChunk / Truncated）
// ============================================================

/// 流式读取一条输出流：读 → 脱敏 → 截断 → 编码 OutputChunk（经 logbuf 编号 seq）。
/// EOF 时 flush 脱敏器暂留窗口（跨块边界的机密前缀在此补齐或作明文外发）。
///
/// `drain` 是进程终态信号（[`run_streamed_step`] 在 `wait_until` 返回后置位）。
/// 但进程退出 ≠ 输出已消费——读任务可能尚未调度，管道缓冲里还压着未读走的
/// 合法输出；孤儿进程（Windows 继承句柄）持有写端时 `read` 更会永不 EOF。
/// 故 drain 置位后不直接放弃，而是转入**限时收尾读**（[`DRAIN_TAIL_WINDOW`]）：
/// 把已缓冲的残留输出读出落库，窗口内无新数据（孤儿持写端阻塞）才放弃——
/// 兼顾「不被残留进程拖到自然退出（如 `ping -n 30` 跑满 30s）」与「终态后
/// 尾段日志不丢」。
//
// 参数多于 clippy 阈值：stream/tag/drain 是 per-流、余下是 per-job 编码上下文
// （脱敏集/截断/标识/日志）——皆流式编码所需，与 run_streamed_step 同纪律。
#[allow(clippy::too_many_arguments)]
async fn stream_output<R>(
    stream: Option<R>,
    tag: Stream,
    secrets: Vec<Vec<u8>>,
    trunc: Arc<Truncation>,
    job_id: String,
    attempt: i32,
    logbuf: LogBuffer,
    mut drain: watch::Receiver<bool>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = match stream {
        Some(r) => r,
        None => return,
    };
    let mut redactor = Redactor::new(secrets);
    let mut buf = vec![0u8; READ_BUF];
    // drain 置位（进程终态）后转收尾读：deadline 限时，孤儿持写端不致久等。
    let mut drained = false;
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        let n = if drained {
            // 收尾读：EOF/错误/窗口耗尽即止。正常路径（无孤儿）管道缓冲读空
            // 后立即 EOF，几乎零等待；阻塞在窗口上的是孤儿持写端的场景。
            let Some(dl) = deadline else { break };
            let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, reader.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => n,
                Err(_elapsed) => break,
            }
        } else {
            tokio::select! {
                n = reader.read(&mut buf) => match n {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                },
                // 进程已终态（wait_until 返回）：转限时收尾读（残留输出继续读出）。
                _ = drain.wait_for(|c| *c) => {
                    drained = true;
                    deadline =
                        Some(tokio::time::Instant::now() + DRAIN_TAIL_WINDOW);
                    continue;
                }
            }
        };
        let redacted = redactor.process(&buf[..n]);
        emit_output(&logbuf, &job_id, attempt, tag, &redacted, &trunc).await;
    }
    let flushed = redactor.flush();
    if !flushed.is_empty() {
        emit_output(&logbuf, &job_id, attempt, tag, &flushed, &trunc).await;
    }
}

/// 编码一段（已脱敏）输出字节为 OutputChunk + 必要的 Truncated 标记，经 logbuf
/// 批量发出（seq 由 logbuf 编号）。
async fn emit_output(
    logbuf: &LogBuffer,
    job_id: &str,
    attempt: i32,
    tag: Stream,
    data: &[u8],
    trunc: &Truncation,
) {
    if data.is_empty() {
        return;
    }
    let (emit_bytes, mark_truncated) = trunc.acquire(data.len());
    let mut events = Vec::with_capacity(2);
    if emit_bytes > 0 {
        events.push(LogEvent {
            seq: 0,
            kind: Some(EventKind::Output(OutputChunk {
                stream: tag as i32,
                data: data[..emit_bytes].to_vec(),
            })),
        });
    }
    if mark_truncated {
        let dropped = (data.len() - emit_bytes) as u64;
        events.push(LogEvent {
            seq: 0,
            kind: Some(EventKind::Truncated(Truncated {
                dropped_bytes: dropped,
            })),
        });
    }
    if !events.is_empty() {
        let _ = logbuf.append_batch(job_id, attempt, events).await;
    }
}

// ============================================================
// run_streamed_step：起进程 → 流式编码 → wait（取消/超时竞争）
// ============================================================

/// 执行一个已起的进程并流式编码其输出：后台写 stdin（可选，svn
/// `--password-from-stdin`）+ stdout/stderr 经脱敏 + 截断编码 + 与取消/超时
/// 竞争 wait。返回进程终态（[`StepOutcome`]），由调用方映射到 job 终态。
///
/// `stdin = Some(data)` 要求 `spawned` 以 `pipe_stdin=true` 起的（[`crate::exec`]
/// 已接管 stdin）；后台写 + 关闭，写失败忽略（进程可能早退报错，stdin 写
/// 拿 EPIPE 不应判败整个步骤）。`timeout = None` = 仅与 cancel 竞争。
//
// 参数多于 clippy 阈值：spawned/stdin/timeout/cancel 是 per-进程、余下是 per-job
// 上下文（脱敏集/截断/标识/日志）——皆流式编码所需，未聚成结构体以保调用点直观。
#[allow(clippy::too_many_arguments)]
pub async fn run_streamed_step(
    mut spawned: SpawnedStep,
    stdin: Option<Vec<u8>>,
    secrets: Vec<Vec<u8>>,
    trunc: Arc<Truncation>,
    job_id: &str,
    attempt: i32,
    timeout: Option<Duration>,
    cancel: watch::Receiver<bool>,
    logbuf: &LogBuffer,
) -> StepOutcome {
    // stdin（svn --password-from-stdin）：取句柄 → 后台写 + 关闭。
    let stdin_task = match (stdin, spawned.take_stdin()) {
        (Some(data), Some(mut sx)) => Some(tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = sx.write_all(&data).await;
            let _ = sx.shutdown().await;
        })),
        _ => None,
    };
    // drain 信号：进程终态（wait_until 返回）后置位，读流任务据此退出——
    // 覆盖「残留子进程持有管道写端、read 阻塞到 EOF」的 Windows 场景。
    let (drain_tx, drain_rx) = watch::channel(false);
    let (stdout, stderr) = spawned.take_streams();
    let out_task = tokio::spawn(stream_output(
        stdout,
        Stream::Stdout,
        secrets.clone(),
        trunc.clone(),
        job_id.to_string(),
        attempt,
        logbuf.clone(),
        drain_rx.clone(),
    ));
    let err_task = tokio::spawn(stream_output(
        stderr,
        Stream::Stderr,
        secrets,
        trunc,
        job_id.to_string(),
        attempt,
        logbuf.clone(),
        drain_rx,
    ));
    let outcome = spawned.wait_until(timeout, cancel).await;
    // 进程已终态：置位 drain 让读流任务尽快退出（不再等 EOF）。随后有界
    // 回收读流任务（正常路径下它们已在 drain 后很快结束；超时仅兜底极端
    // 调度——读循环在 drain 竞争点响应）。
    drain_tx.send(true).ok();
    let _ = tokio::time::timeout(STREAM_DRAIN_TIMEOUT, out_task).await;
    let _ = tokio::time::timeout(STREAM_DRAIN_TIMEOUT, err_task).await;
    if let Some(t) = stdin_task {
        let _ = t.await;
    }
    outcome
}

/// 进程终态后读流任务的回收时限：drain 信号置位后给已产出输出落库的时间，
/// 超时即放弃 join（读循环在 drain 竞争点响应，超时仅兜底极端调度）。
const STREAM_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 进程终态（drain 置位）后残留输出的收尾读窗口：管道缓冲里未读走的输出
/// 继续读出，窗口内无新数据（孤儿进程持写端阻塞 read）才放弃。须小于
/// [`STREAM_DRAIN_TIMEOUT`] 的 join 预算——给收尾读后的 flush 落库留时间。
const DRAIN_TAIL_WINDOW: Duration = Duration::from_secs(1);

// ============================================================
// 单元测试（Truncation 纯逻辑，从 runner 迁移）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::agent::channel_message::Kind;

    #[test]
    fn truncation_emits_until_limit_then_marker_then_drops() {
        let trunc = Truncation::new(10);
        // 前两块各 4 字节 → 全发，无标记。
        assert_eq!(trunc.acquire(4), (4, false));
        assert_eq!(trunc.acquire(4), (4, false));
        // 第三块 4 字节：配额剩 2 → 发 2 + 标记（dropped 2）。
        assert_eq!(trunc.acquire(4), (2, true));
        // 后续全丢、不再标记。
        assert_eq!(trunc.acquire(8), (0, false));
        assert_eq!(trunc.acquire(8), (0, false));
    }

    #[test]
    fn truncation_exact_limit_no_marker() {
        let trunc = Truncation::new(4);
        assert_eq!(trunc.acquire(4), (4, false), "恰好到上限不发标记");
        assert_eq!(trunc.acquire(1), (0, true), "越限首块发标记、0 字节");
        assert_eq!(trunc.acquire(1), (0, false));
    }

    #[test]
    fn truncation_single_chunk_exceeding_limit() {
        let trunc = Truncation::new(3);
        // 一块 10 字节：发 3 + 标记（dropped 7）。
        assert_eq!(trunc.acquire(10), (3, true));
        assert_eq!(trunc.acquire(1), (0, false));
    }

    /// 回归（windows-latest CI 红）：进程终态（drain 置位）时读任务可能
    /// 尚未把管道缓冲里的输出读走——收尾读必须把它读出，不得直接丢弃。
    /// 场景钉法：drain 先置位、reader 尚无数据，随后数据才到——旧实现
    /// （drain 即 break）在数据到达前就已退出，新实现在窗口内等并读出。
    #[tokio::test]
    async fn drain_settled_still_emits_buffered_tail_output() {
        let dir = tempfile::tempdir().expect("临时缓冲目录");
        let logbuf = LogBuffer::new(dir.path().to_path_buf(), crate::logbuf::DEFAULT_GRACE);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        logbuf.set_live(Some(tx)).await;

        // duplex 模拟进程管道：writer 未写、reader 未读；drain 已置位
        // （进程已终态、读任务落后）。
        let (mut writer, reader) = tokio::io::duplex(64);
        let (drain_tx, drain_rx) = watch::channel(false);
        drain_tx.send(true).expect("drain 置位");

        let trunc = Arc::new(Truncation::new(1 << 20));
        let task = tokio::spawn(stream_output(
            Some(reader),
            Stream::Stdout,
            vec![],
            trunc,
            "job-tail".to_string(),
            0,
            logbuf.clone(),
            drain_rx,
        ));

        // 旧实现在此已退出（drain 即 break）；新实现应留在收尾读窗口内。
        tokio::time::sleep(Duration::from_millis(100)).await;
        use tokio::io::AsyncWriteExt;
        writer.write_all(b"tail-output").await.expect("写入尾段");
        drop(writer); // EOF：正常路径收尾读到 EOF 即止，不等窗口耗尽。

        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("读流任务应在收尾窗口内结束")
            .expect("join 读流任务");

        // 尾段输出必须已脱敏外发（落盘 + 活体转发同路）。
        let mut collected = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Some(Kind::LogBatch(batch)) = msg.kind {
                for ev in batch.events {
                    if let Some(EventKind::Output(o)) = ev.kind {
                        collected.extend_from_slice(&o.data);
                    }
                }
            }
        }
        assert!(
            collected.ends_with(b"tail-output"),
            "drain 置位后到达的残留输出不得丢失：{collected:?}"
        );
    }
}

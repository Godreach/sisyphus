//! Agent 侧日志 seq 缓冲（ADR-0007/0013；票 B3-T3）。
//!
//! 每 (job_id, attempt) 一个缓冲文件 `<data>/logbuf/<job_id>-<attempt>.jsonl`。
//! 日志事件先落盘（批量 fsync）再经通道发出（[`LogBuffer::append`]/[`LogBuffer::append_batch`]：
//! 写盘后若有活体连接即转发）；断线期间缓冲继续累计——补传语义 = 幂等重放，
//! Server 按 seq 落库天然幂等（重复 seq 覆盖/忽略），Agent 不做 per-batch ack 等待。
//!
//! - **seq**：per (job, attempt) 单调分配（[`LogBuffer`] 内部按文件行数恢复
//!   `next_seq`，attempt 重置互不污染）。每条事件由本模块编号后写入缓冲行。
//! - **落盘纪律**：`append_batch` 攒一批行后一次 `sync_all`（批量 fsync）；
//!   单事件 `append` 同样落盘 + 同步。断线 = 无活体转发，缓冲继续 `append`
//!   落盘——断线期间不丢。
//! - **重放**：[`LogBuffer::replay_all`] 从每个缓冲文件头逐行读到 EOF（缓冲只
//!   删除不截断，重放天然「从起点重发未清空段」）。每次重连都从文件头重放，
//!   已送达段重复重放由 Server 侧幂等吸收。
//! - **删除纪律**（ADR-0013）：终态上报成功后 [`LogBuffer::clear_deferred`]
//!   延迟固定宽限（默认 [`DEFAULT_GRACE`] 1 分钟）删除——宽限内崩溃重启缓冲
//!   留作重启后孤儿补传取证；孤儿缓冲（Agent 重启遗留、任务已判 aborted）在
//!   重连 JobReported(aborted) 补传完成后 [`LogBuffer::clear_now`] 删除——执行
//!   丢弃、日志保留作取证后清空。
//! - **行形态**：每行 JSON = 一条日志事件 `{"seq": n, "kind": ..., ...}`，输出
//!   字节以 base64 编码落盘（jsonl 行内保二进制安全；ANSI 色码原样，ADR-0013）。
//!   重放时反解回 proto `LogEvent` 并包成 `LogBatch` 帧（job_id/attempt 来自
//!   重放调用参数）。
//!
//! 组合根持一个 [`LogBuffer`]：runner（#59）经 `append`/`append_batch` 喂事件
//! （喂给本模块的 `LogEvent.seq` 会被本模块重新编号——seq 分配归缓冲层），
//! 通道经 `set_live`/`replay_all`/`clear_now` 驱动补传与孤儿清理。

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64ct::{Base64UrlUnpadded, Encoding};
use sisyphus_proto::agent::log_event::Kind as EventKind;
use sisyphus_proto::agent::{
    ChannelMessage, LogBatch, LogEvent, OutputChunk, StepEvent, Truncated, channel_message::Kind,
};
use tokio::sync::RwLock;
use tokio::sync::mpsc;

/// 缓冲删除宽限默认值（ADR-0013：终态上报成功后延迟固定宽限删除，默认
/// 1 分钟——宽限内崩溃重启缓冲留作孤儿取证）。
pub const DEFAULT_GRACE: Duration = Duration::from_secs(60);
/// 缓冲文件后缀（jsonl = 每行一个 JSON）。
const LOGBUF_EXT: &str = ".jsonl";
/// 单 (job, attempt) 行缓冲容量（OS 写缓冲；fsync 节奏由调用方 batch 控制）。
const LINE_BUF_CAPACITY: usize = 64 * 1024;

/// 单 (job, attempt) 缓冲写句柄：文件 + 行缓冲 + 下一 seq。
struct JobBuffer {
    file: File,
    writer: BufWriter<File>,
    next_seq: u64,
}

impl JobBuffer {
    /// 打开（或新建）缓冲文件：追加写；`next_seq` 从文件行数恢复（重启后
    /// 续跑——seq 从头计，attempt 内单调连续）。
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let next_seq = count_lines(path)?;
        let writer = BufWriter::with_capacity(LINE_BUF_CAPACITY, file.try_clone()?);
        Ok(Self {
            file,
            writer,
            next_seq,
        })
    }

    /// 追加一行 JSON（事件）+ 换行，flush 到 OS 并 fsync 落盘。
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.file.sync_all()
    }
}

/// 删除调度的内部指令（缓冲文件路径 + 延迟）。
struct DeleteJob {
    path: PathBuf,
    delay: Duration,
}

/// 日志缓冲管理器（[`Clone`]：多模块共享同一缓冲目录与删除调度）。
#[derive(Clone)]
pub struct LogBuffer {
    dir: PathBuf,
    grace: Duration,
    /// 打开中的 (job_id, attempt) → 写句柄。每 job 由 runner 唯一写者驱动；
    /// 此处防同 (job, attempt) 重复 open（重试竞态）并回收句柄供删除。
    open: Arc<Mutex<HashMap<(String, i32), JobBuffer>>>,
    /// 活体连接的上行发送器（通道 `set_live` 注入；`None` = 断线，事件仅落盘）。
    live: Arc<RwLock<Option<mpsc::Sender<ChannelMessage>>>>,
    /// 缓冲删除调度（延迟宽限删除经此；`clear_now` 同步删除不走此）。
    delete_tx: mpsc::Sender<DeleteJob>,
}

impl LogBuffer {
    /// 以缓冲目录与删除宽限构造。目录须已存在（config 层创建）。**须在
    /// tokio runtime 上下文中调用**（删除调度 worker 在此 spawn；组合根与
    /// 测试均满足）。
    pub fn new(dir: PathBuf, grace: Duration) -> Self {
        let (delete_tx, mut delete_rx) = mpsc::channel::<DeleteJob>(64);
        tokio::spawn(async move {
            while let Some(job) = delete_rx.recv().await {
                if job.delay.is_zero() {
                    remove_file_best_effort(&job.path);
                } else {
                    tokio::time::sleep(job.delay).await;
                    remove_file_best_effort(&job.path);
                }
            }
        });
        Self {
            dir,
            grace,
            open: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(RwLock::new(None)),
            delete_tx,
        }
    }

    /// 缓冲文件路径：`<dir>/<job_id>-<attempt>.jsonl`。
    pub fn path(&self, job_id: &str, attempt: i32) -> PathBuf {
        self.dir.join(format!("{job_id}-{attempt}{LOGBUF_EXT}"))
    }

    /// 缓冲目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 追加一条日志事件：分配 seq → 落盘（含 fsync）→ 若有活体连接即转发。
    /// 返回对应的 `LogBatch` 帧（seq 由本模块编号；调用方无须再发送——转发
    /// 已由本方法完成，返回值供测试/回执使用）。
    pub async fn append(
        &self,
        job_id: &str,
        attempt: i32,
        event: LogEvent,
    ) -> std::io::Result<ChannelMessage> {
        let msg = self.write_event(job_id, attempt, event)?;
        self.forward_live(&msg).await;
        Ok(msg)
    }

    /// 批量追加：同 (job, attempt) 的多条事件一次落盘（批量 fsync——写完全部
    /// 行后一次同步）。帧序即写入序，seq 连续单调。返回逐条帧。
    pub async fn append_batch(
        &self,
        job_id: &str,
        attempt: i32,
        events: Vec<LogEvent>,
    ) -> std::io::Result<Vec<ChannelMessage>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let msgs = self.write_batch(job_id, attempt, events)?;
        for msg in &msgs {
            self.forward_live(msg).await;
        }
        Ok(msgs)
    }

    /// 单事件落盘（写盘部分——不持锁跨 await）。
    fn write_event(
        &self,
        job_id: &str,
        attempt: i32,
        event: LogEvent,
    ) -> std::io::Result<ChannelMessage> {
        let key = (job_id.to_string(), attempt);
        let mut open = self.open.lock().expect("缓冲表锁");
        let buf = ensure_job_buffer(&mut open, &key, &self.path(job_id, attempt))?;
        let seq = buf.next_seq;
        buf.next_seq += 1;
        let msg = ChannelMessage {
            kind: Some(Kind::LogBatch(LogBatch {
                job_id: job_id.to_string(),
                attempt,
                start_seq: seq,
                events: vec![event_with_seq(event, seq)],
            })),
        };
        let line = event_to_json(seq, log_event_of(&msg).expect("刚构造的帧必有事件"));
        buf.write_line(&line)?;
        Ok(msg)
    }

    /// 批量落盘（批量 fsync）。
    fn write_batch(
        &self,
        job_id: &str,
        attempt: i32,
        events: Vec<LogEvent>,
    ) -> std::io::Result<Vec<ChannelMessage>> {
        let key = (job_id.to_string(), attempt);
        let mut open = self.open.lock().expect("缓冲表锁");
        let buf = ensure_job_buffer(&mut open, &key, &self.path(job_id, attempt))?;
        let start = buf.next_seq;
        buf.next_seq += events.len() as u64;
        let msgs: Vec<ChannelMessage> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| {
                let seq = start + i as u64;
                ChannelMessage {
                    kind: Some(Kind::LogBatch(LogBatch {
                        job_id: job_id.to_string(),
                        attempt,
                        start_seq: seq,
                        events: vec![event_with_seq(event, seq)],
                    })),
                }
            })
            .collect();
        let mut lines = String::new();
        for msg in &msgs {
            lines.push_str(&event_to_json(
                batch_start(msg),
                log_event_of(msg).expect("本方法构造的帧必有事件"),
            ));
            lines.push('\n');
        }
        buf.write_line(&lines)?;
        Ok(msgs)
    }

    /// 活体转发：连接在线（`set_live(Some)`）时把帧送入上行邮箱（单 writer
    /// 保序）；断线（`None`）时不送——事件已落盘，重连重放补传。
    async fn forward_live(&self, msg: &ChannelMessage) {
        let live = self.live.read().await;
        if let Some(tx) = live.as_ref()
            && tx.send(msg.clone()).await.is_err()
        {
            // 连接刚断（对端关流）：事件已落盘，重连重放兜底，不判败。
            tracing::warn!("日志活体转发失败：通道已关闭，事件仅落缓冲");
        }
    }

    /// 注入/清除活体连接的上行发送器（通道重连时调用）。`set_live(Some)` 须
    /// 先于 `replay_all`：连接期追加的活体转发与重放重复段由 Server 幂等吸收，
    /// 避免「重放读完 → 活体注入」窗口内的漏发。
    pub async fn set_live(&self, tx: Option<mpsc::Sender<ChannelMessage>>) {
        *self.live.write().await = tx;
    }

    /// 幂等重放单文件：从缓冲起点（文件头）逐行读到 EOF，产出 `LogBatch` 帧
    /// 序列（seq 连续、按行序）。文件不存在/为空 = 无缓冲，返回空。
    ///
    /// 与追加互斥（防撕裂读半截行）；半截行（崩溃窗口）丢弃该行及之后——
    /// 崩溃窗口内不会出现完整行位于半截行之后（顺序追加 + fsync），重放
    /// 在首个损坏行停止不损失有效段。
    pub async fn replay(&self, job_id: &str, attempt: i32) -> std::io::Result<Vec<ChannelMessage>> {
        let path = self.path(job_id, attempt);
        let _guard = self.open.lock().expect("缓冲表锁");
        self.read_file(&path, job_id, attempt)
    }

    /// 重放全部缓冲文件（重连补传入口）：按目录顺序逐文件重放，产出全部
    /// `LogBatch` 帧。调用方（channel 重连路径）把帧按序发往 writer，与活体
    /// 日志同一 writer（单 writer 保序）。
    pub async fn replay_all(&self) -> std::io::Result<Vec<ChannelMessage>> {
        let mut out = Vec::new();
        for (job, attempt) in self.buffer_files() {
            out.extend(self.replay(&job, attempt).await?);
        }
        Ok(out)
    }

    /// 孤儿检测：缓冲目录里 job_id 不在在途集内的 (job, attempt) 即孤儿
    /// （Agent 重启遗留、任务已判 aborted——执行丢弃、日志保留作取证）。
    pub fn orphans(&self, in_flight: &[String]) -> Vec<(String, i32)> {
        let in_flight: HashSet<&str> = in_flight.iter().map(String::as_str).collect();
        self.buffer_files()
            .into_iter()
            .filter(|(job, _)| !in_flight.contains(job.as_str()))
            .collect()
    }

    /// 延迟删除缓冲文件（终态上报成功后的宽限删除）。宽限为 0 = 立即经
    /// worker 删除。发送失败（worker 已退出）记警告——删除是维护动作，失败
    /// 只留孤儿文件，不阻塞数据路径。
    pub fn clear_deferred(&self, job_id: &str, attempt: i32) {
        self.release_handle(job_id, attempt);
        let _ = self.delete_tx.try_send(DeleteJob {
            path: self.path(job_id, attempt),
            delay: self.grace,
        });
    }

    /// 立即删除缓冲文件（孤儿补传完成后——执行丢弃、日志保留作取证后清空）。
    /// 同步删除：先回收写句柄（drop File，Windows 可删）再删文件，删除完成
    /// 才返回——避免与「同路径重建缓冲」竞态。
    pub fn clear_now(&self, job_id: &str, attempt: i32) {
        self.release_handle(job_id, attempt);
        remove_file_best_effort(&self.path(job_id, attempt));
    }

    /// 回收 (job, attempt) 的写句柄（drop File——删除前必须，Windows 不能删
    /// 打开中的文件）。
    fn release_handle(&self, job_id: &str, attempt: i32) {
        self.open
            .lock()
            .expect("缓冲表锁")
            .remove(&(job_id.to_string(), attempt));
    }

    /// 读文件逐行重放为帧（同步；调用方持有 open 锁防撕裂）。
    fn read_file(
        &self,
        path: &Path,
        job_id: &str,
        attempt: i32,
    ) -> std::io::Result<Vec<ChannelMessage>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(event) = event_from_json(&line) else {
                tracing::warn!(
                    job = %job_id,
                    attempt,
                    path = %path.display(),
                    "缓冲行损坏，停止重放该文件（崩溃窗口，seq 由 Server 幂等吸收）"
                );
                break;
            };
            let seq = event.seq;
            out.push(ChannelMessage {
                kind: Some(Kind::LogBatch(LogBatch {
                    job_id: job_id.to_string(),
                    attempt,
                    start_seq: seq,
                    events: vec![event],
                })),
            });
        }
        Ok(out)
    }

    /// 枚举缓冲目录内全部 (job_id, attempt)（按目录顺序；解析失败的条目跳过）。
    fn buffer_files(&self) -> Vec<(String, i32)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(LOGBUF_EXT)?;
                let (job, attempt) = stem.rsplit_once('-')?;
                let attempt = attempt.parse::<i32>().ok()?;
                Some((job.to_string(), attempt))
            })
            .collect()
    }
}

/// 取/建 (job, attempt) 的写句柄（无则打开文件——首个 open 恢复 next_seq）。
fn ensure_job_buffer<'a>(
    open: &'a mut HashMap<(String, i32), JobBuffer>,
    key: &(String, i32),
    path: &Path,
) -> std::io::Result<&'a mut JobBuffer> {
    if !open.contains_key(key) {
        open.insert(key.clone(), JobBuffer::open(path)?);
    }
    Ok(open.get_mut(key).expect("刚插入的句柄"))
}

/// 把事件与分配的 seq 合成 `LogEvent`（seq 由缓冲层裁决，覆盖调用方传入值）。
fn event_with_seq(event: LogEvent, seq: u64) -> LogEvent {
    LogEvent {
        seq,
        kind: event.kind,
    }
}

/// 从 LogBatch 帧取单事件（本模块构造的帧恒单事件）。
fn log_event_of(msg: &ChannelMessage) -> Option<&LogEvent> {
    match msg.kind.as_ref()? {
        Kind::LogBatch(b) => b.events.first(),
        _ => None,
    }
}

/// 取 LogBatch 帧的起始 seq。
fn batch_start(msg: &ChannelMessage) -> u64 {
    match msg.kind.as_ref().expect("日志帧") {
        Kind::LogBatch(b) => b.start_seq,
        _ => 0,
    }
}

/// 事件 → 缓冲行 JSON（`{"seq": n, "kind": "output|step|truncated", ...}`；
/// 输出字节 base64——jsonl 行内保二进制安全）。
fn event_to_json(seq: u64, event: &LogEvent) -> String {
    use serde_json::{Value, json};
    let mut obj = serde_json::Map::new();
    obj.insert("seq".into(), json!(seq));
    match event.kind.as_ref() {
        Some(EventKind::Output(o)) => {
            obj.insert("kind".into(), json!("output"));
            obj.insert("stream".into(), json!(o.stream));
            obj.insert(
                "data".into(),
                json!(Base64UrlUnpadded::encode_string(&o.data)),
            );
        }
        Some(EventKind::Step(s)) => {
            obj.insert("kind".into(), json!("step"));
            obj.insert("step_seq".into(), json!(s.seq));
            obj.insert("started_at".into(), json!(s.step_started_at_ms));
            obj.insert("ended_at".into(), json!(s.step_ended_at_ms));
            obj.insert("exit_code".into(), json!(s.exit_code));
            obj.insert("command".into(), json!(s.command));
        }
        Some(EventKind::Truncated(t)) => {
            obj.insert("kind".into(), json!("truncated"));
            obj.insert("dropped_bytes".into(), json!(t.dropped_bytes));
        }
        None => {
            obj.insert("kind".into(), Value::Null);
        }
    }
    serde_json::to_string(&Value::Object(obj)).expect("JSON 序列化恒可成功")
}

/// 缓冲行 JSON → 事件。行损坏（崩溃窗口/格式演进）返回 `None`——重放丢弃
/// 该行并停止（见 [`LogBuffer::read_file`]）。
fn event_from_json(line: &str) -> Option<LogEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let seq = v.get("seq")?.as_u64()?;
    let kind = match v.get("kind")?.as_str()? {
        "output" => {
            let stream = v.get("stream")?.as_i64()? as i32;
            let data = Base64UrlUnpadded::decode_vec(v.get("data")?.as_str()?).ok()?;
            Some(EventKind::Output(OutputChunk { stream, data }))
        }
        "step" => {
            let seq_no = v.get("step_seq")?.as_i64()? as i32;
            let started = v.get("started_at")?.as_i64()?;
            let ended = v.get("ended_at")?.as_i64()?;
            let exit_code = v
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map(|x| x as i32);
            let command = v.get("command")?.as_str()?.to_string();
            Some(EventKind::Step(StepEvent {
                seq: seq_no,
                step_started_at_ms: started,
                step_ended_at_ms: ended,
                exit_code,
                command,
            }))
        }
        "truncated" => {
            let dropped = v.get("dropped_bytes")?.as_u64()?;
            Some(EventKind::Truncated(Truncated {
                dropped_bytes: dropped,
            }))
        }
        _ => return None,
    };
    Some(LogEvent { seq, kind })
}

/// 删除文件（尽力而为：不存在即忽略；失败记警告——孤儿文件是取证资产，
/// 清不掉比误删好）。
fn remove_file_best_effort(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "缓冲文件已删除"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "缓冲文件删除失败"),
    }
}

/// 统计缓冲文件非空行数（= 已写事件数 = 下一 seq 起点）。文件不存在 = 0。
fn count_lines(path: &Path) -> std::io::Result<u64> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0u64;
    for line in BufReader::new(file).lines() {
        if !line?.trim().is_empty() {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::agent::Stream;

    fn output_event(data: &[u8]) -> LogEvent {
        LogEvent {
            seq: 0, // 缓冲层重编号，此值被覆盖
            kind: Some(EventKind::Output(OutputChunk {
                stream: Stream::Stdout as i32,
                data: data.to_vec(),
            })),
        }
    }

    fn seq_of(msg: &ChannelMessage) -> u64 {
        batch_start(msg)
    }

    fn output_data(msg: &ChannelMessage) -> &[u8] {
        match log_event_of(msg)
            .expect("事件")
            .kind
            .as_ref()
            .expect("kind")
        {
            EventKind::Output(o) => &o.data,
            _ => panic!("非输出事件"),
        }
    }

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_roundtrips() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);

        let m1 = buf
            .append("job-1", 0, output_event(b"hello"))
            .await
            .unwrap();
        let m2 = buf
            .append("job-1", 0, output_event(b"world"))
            .await
            .unwrap();
        assert_eq!(seq_of(&m1), 0, "首个事件 seq=0");
        assert_eq!(seq_of(&m2), 1, "同 (job, attempt) 单调");

        let replayed = buf.replay("job-1", 0).await.unwrap();
        assert_eq!(replayed.len(), 2, "两条都重放");
        assert_eq!(seq_of(&replayed[0]), 0);
        assert_eq!(seq_of(&replayed[1]), 1);
        assert_eq!(output_data(&replayed[0]), b"hello");
        assert_eq!(output_data(&replayed[1]), b"world");
    }

    #[tokio::test]
    async fn attempt_isolation_resets_seq() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);

        buf.append("job-1", 0, output_event(b"a")).await.unwrap();
        buf.append("job-1", 0, output_event(b"b")).await.unwrap();
        let m = buf.append("job-1", 1, output_event(b"c")).await.unwrap();
        assert_eq!(
            seq_of(&m),
            0,
            "attempt=1 从头计 seq（与 attempt=0 互不污染）"
        );

        assert!(buf.path("job-1", 0).exists(), "attempt 0 缓冲独立保留");
        assert!(buf.path("job-1", 1).exists(), "attempt 1 缓冲独立");
        let replayed = buf.replay("job-1", 1).await.unwrap();
        assert_eq!(replayed.len(), 1, "attempt=1 只重放自身");
    }

    #[tokio::test]
    async fn replay_is_empty_for_missing_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        let replayed = buf.replay("job-9", 3).await.unwrap();
        assert!(replayed.is_empty(), "无缓冲文件 = 空重放");
    }

    #[tokio::test]
    async fn buffered_events_survive_and_replay_from_start() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        buf.append("job-1", 0, output_event(b"a")).await.unwrap();
        buf.append("job-1", 0, output_event(b"b")).await.unwrap();
        // 断线续写：缓冲继续累计（重放从文件头 = 未清空段全部）。
        buf.append("job-1", 0, output_event(b"c")).await.unwrap();

        let replayed = buf.replay("job-1", 0).await.unwrap();
        assert_eq!(replayed.len(), 3, "重放全部未清空段（不截断）");
        assert_eq!(output_data(&replayed[2]), b"c");
    }

    #[tokio::test]
    async fn append_batch_fsyncs_and_preserves_order() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        let msgs = buf
            .append_batch(
                "job-1",
                0,
                vec![output_event(b"a"), output_event(b"b"), output_event(b"c")],
            )
            .await
            .unwrap();
        assert_eq!(msgs.len(), 3);
        let seqs: Vec<u64> = msgs.iter().map(seq_of).collect();
        assert_eq!(seqs, vec![0, 1, 2], "批量 seq 连续单调");
        // 已 fsync——重放立即读到全部。
        let replayed = buf.replay("job-1", 0).await.unwrap();
        assert_eq!(replayed.len(), 3);
        assert_eq!(output_data(&replayed[2]), b"c");
    }

    #[tokio::test]
    async fn append_batch_then_single_continue_seq() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        buf.append_batch("job-1", 0, vec![output_event(b"a"), output_event(b"b")])
            .await
            .unwrap();
        let m = buf.append("job-1", 0, output_event(b"c")).await.unwrap();
        assert_eq!(seq_of(&m), 2, "batch 后续单事件接着编号");
    }

    #[tokio::test]
    async fn base64_roundtrips_binary_output() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        let binary = vec![0u8, 1, 2, 255, 254, b'\n', b'"', 0x80];
        buf.append("job-1", 0, output_event(&binary)).await.unwrap();
        let replayed = buf.replay("job-1", 0).await.unwrap();
        assert_eq!(
            output_data(&replayed[0]),
            binary.as_slice(),
            "二进制字节原样往返"
        );
    }

    #[tokio::test]
    async fn step_and_truncated_events_roundtrip() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        buf.append(
            "job-1",
            0,
            LogEvent {
                seq: 0,
                kind: Some(EventKind::Step(StepEvent {
                    seq: 2,
                    step_started_at_ms: 100,
                    step_ended_at_ms: 150,
                    exit_code: Some(0),
                    command: "echo hi".into(),
                })),
            },
        )
        .await
        .unwrap();
        buf.append(
            "job-1",
            0,
            LogEvent {
                seq: 0,
                kind: Some(EventKind::Truncated(Truncated {
                    dropped_bytes: 4096,
                })),
            },
        )
        .await
        .unwrap();
        let replayed = buf.replay("job-1", 0).await.unwrap();
        assert_eq!(replayed.len(), 2);
        match log_event_of(&replayed[0])
            .expect("事件")
            .kind
            .as_ref()
            .expect("kind")
        {
            EventKind::Step(s) => {
                assert_eq!(s.seq, 2);
                assert_eq!(s.exit_code, Some(0));
                assert_eq!(s.command, "echo hi");
            }
            _ => panic!("期望 step 事件"),
        }
        match log_event_of(&replayed[1])
            .expect("事件")
            .kind
            .as_ref()
            .expect("kind")
        {
            EventKind::Truncated(t) => assert_eq!(t.dropped_bytes, 4096),
            _ => panic!("期望 truncated 事件"),
        }
    }

    #[tokio::test]
    async fn clear_deferred_removes_after_grace() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), Duration::from_millis(20));
        buf.append("job-1", 0, output_event(b"a")).await.unwrap();
        assert!(buf.path("job-1", 0).exists());

        buf.clear_deferred("job-1", 0);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!buf.path("job-1", 0).exists(), "宽限到期删除");
    }

    #[tokio::test]
    async fn clear_now_removes_immediately_and_release_handle() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        buf.append("job-1", 0, output_event(b"a")).await.unwrap();
        assert!(buf.path("job-1", 0).exists());

        buf.clear_now("job-1", 0);
        assert!(!buf.path("job-1", 0).exists(), "同步立即删除");
        // 删除后同路径重建：seq 从头计（新文件）。
        let m = buf.append("job-1", 0, output_event(b"b")).await.unwrap();
        assert_eq!(seq_of(&m), 0, "重建缓冲从 seq 0 起");
    }

    #[tokio::test]
    async fn orphans_lists_only_buffers_not_in_flight() {
        let dir = tempfile::tempdir().expect("临时目录");
        let buf = LogBuffer::new(dir.path().to_path_buf(), DEFAULT_GRACE);
        buf.append("job-1", 0, output_event(b"a")).await.unwrap();
        buf.append("job-2", 1, output_event(b"b")).await.unwrap();
        buf.append("job-2", 2, output_event(b"c")).await.unwrap();

        let orphans = buf.orphans(&["job-1".to_string()]);
        assert_eq!(orphans.len(), 2, "job-1 在途，job-2 两个 attempt 是孤儿");
        assert!(orphans.contains(&("job-2".to_string(), 1)));
        assert!(orphans.contains(&("job-2".to_string(), 2)));
    }
}

//! 日志事件模型与 chunk 编解码（票 #73 / B5-T1，ADR-0013）。
//!
//! 流结构：带类型的事件流，stdout/stderr 合流；流元素 = 输出块（带 stream
//! 标记）+ 步骤生命周期事件（step start 含命令回显 / step end 含退出码与
//! 耗时）+ 截断标记，单一有序序列按到达顺序交织、per-job 单调 seq 定位
//!（按 attempt 计）。`job_end` 是 SSE 层的派生事件（任务终态合成、不落库）。
//!
//! **chunk 编码**：一个 [`LogChunk`] = gzip 压缩的 JSONL（每行一条
//! [`LogStreamEvent`]，serde 形态与前端 `sse.ts` 的解析契约逐字对齐：`type`
//! tag + snake_case 字段）。grpc 侧把 proto `LogEvent` 映射为本模型后编码
//! 落库；SSE 回放 / 整份下载侧解压解码。每块独立压缩（范围读取解压互不
//! 依赖，ADR-0013）。gzip 走 flate2 纯 Rust 后端（miniz_oxide，不引 zstd）。
//!
//! 输出字节原样存储（含 ANSI 色码，前端纯文本渲染时剥离）；proto 输出是
//! 任意字节，本模型 `text` 为 UTF-8 有损解码（SSE/JSON 传输面是文本）。

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

use crate::store::{LogChunk, LogLocation};

/// per-job 日志上限默认值（ADR-0013：Server 全局配置，默认 50 MB；v1 无
/// 覆盖面，截断标记事件的 `limit_bytes` 取此值）。
pub const DEFAULT_LOG_LIMIT_BYTES: u64 = 50 * 1024 * 1024;

/// 输出流标记（stdout/stderr 合流，ADR-0013）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    /// 标准输出。
    Stdout,
    /// 标准错误（合流保序，UI 醒目标记）。
    Stderr,
}

/// 落库的日志流事件（单一有序序列；serde 形态与前端 `sse.ts` 契约对齐）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogStreamEvent {
    /// 输出块（带 stream 标记；ANSI 色码原样，渲染侧剥离）。
    Output {
        /// 本事件 seq。
        seq: u64,
        /// stdout/stderr 合流标记。
        stream: LogStream,
        /// 输出文本（原始字节 UTF-8 有损解码）。
        text: String,
    },
    /// 步骤开始（含命令回显，ADR-0013）。
    StepStart {
        /// 本事件 seq。
        seq: u64,
        /// 步骤序号（从 0 起）。
        step: i32,
        /// 步骤名（v1 proto 不携带，恒空——前端回落「步骤 N」）。
        #[serde(default)]
        name: String,
        /// 命令回显（Agent 始终回显步骤命令行进日志）。
        command: String,
        /// 步骤开始时刻（Unix 毫秒）。
        started_at: i64,
    },
    /// 步骤结束（退出码与耗时）。
    StepEnd {
        /// 本事件 seq。
        seq: u64,
        /// 步骤序号（与 step start 对应）。
        step: i32,
        /// 退出码（可空）。
        exit_code: Option<i32>,
        /// 耗时（毫秒）。
        duration_ms: i64,
    },
    /// 截断标记（超限丢弃、不判败，UI 显著标注，ADR-0013）。
    Truncated {
        /// 本事件 seq。
        seq: u64,
        /// 触发截断的日志上限（字节）。
        limit_bytes: u64,
        /// 丢弃的字节数（信息面；前端契约外字段）。
        dropped_bytes: u64,
    },
}

/// SSE 层合成的任务终态事件（任务终态送达并 flush 后关流；不落库——
/// 任务状态在 jobs 行，SSE 端点从行状态合成）。字段形态与前端
/// `sse.ts` 契约对齐（含 `type` tag）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEndEvent {
    /// 事件类型（恒 "job_end"，前端 parseLogEvent 按此分派；经
    /// [`JobEndEvent::new`] 构造保证）。
    #[serde(rename = "type")]
    pub kind: String,
    /// 本事件 seq（末尾日志 seq + 1，续传游标语义）。
    pub seq: u64,
    /// 任务终态（succeeded/failed/cancelled/timeout/aborted/skipped）。
    pub status: String,
    /// 退出码（可空）。
    pub exit_code: Option<i32>,
}

impl JobEndEvent {
    /// 合成任务终态事件。
    pub fn new(seq: u64, status: &str, exit_code: Option<i32>) -> Self {
        Self {
            kind: "job_end".into(),
            seq,
            status: status.into(),
            exit_code,
        }
    }
}

/// chunk 编解码错误。
#[derive(Debug)]
pub enum LogCodecError {
    /// gzip 解压失败（IO）。
    Io(std::io::Error),
    /// JSONL 行损坏（非本模型的 JSON）。
    BadLine(String),
}

impl std::fmt::Display for LogCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "日志 chunk 解压失败：{e}"),
            Self::BadLine(line) => write!(f, "日志 chunk 行损坏：{line}"),
        }
    }
}

impl std::error::Error for LogCodecError {}

impl From<std::io::Error> for LogCodecError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 事件序列编码为 chunk：JSONL → gzip（每块独立压缩，ADR-0013）。
/// `start_seq` 取首事件 seq（空序列由调用侧跳过——空 chunk 无意义）。
pub fn encode_chunk(events: &[LogStreamEvent]) -> LogChunk {
    let mut jsonl = String::new();
    for ev in events {
        jsonl.push_str(&serde_json::to_string(ev).expect("日志事件 JSON 恒可序列化"));
        jsonl.push('\n');
    }
    LogChunk {
        start_seq: events.first().map(|e| e.seq()).unwrap_or(0),
        compressed: gzip(jsonl.as_bytes()),
    }
}

/// chunk 解码为事件序列（gzip 解压互不依赖；行损坏取 `Err`——落库内容
/// 出自本 codec，损坏即库异常，调用侧记日志跳过该 chunk 不炸流）。
pub fn decode_chunk(chunk: &LogChunk) -> Result<Vec<LogStreamEvent>, LogCodecError> {
    let raw = gunzip(&chunk.compressed)?;
    let text = String::from_utf8_lossy(&raw);
    let mut events = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: LogStreamEvent =
            serde_json::from_str(line).map_err(|_| LogCodecError::BadLine(line.to_string()))?;
        events.push(ev);
    }
    Ok(events)
}

/// chunk 元数据（logs 表的派生列）：end_seq = 末事件 seq；step = chunk 内
/// 步骤事件一致的步骤序号（否则 -1）；stream = chunk 内输出块一致的
/// stream 标记（否则空串）。落库时由 store 提取作查询面冗余列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMeta {
    /// 本 chunk 末事件 seq。
    pub end_seq: u64,
    /// 一致的步骤序号（无/混合为 -1）。
    pub step: i32,
    /// 一致的 stream 标记（无/混合为空串）。
    pub stream: &'static str,
}

/// 提取 chunk 元数据（解压一次；损坏 chunk 抛错由调用侧裁决）。
pub fn chunk_meta(events: &[LogStreamEvent]) -> ChunkMeta {
    let end_seq = events.last().map(|e| e.seq()).unwrap_or(0);
    // 步骤序号：全部步骤事件一致才取值，否则 -1（无/混合）。
    let steps: Vec<i32> = events
        .iter()
        .filter_map(|ev| match ev {
            LogStreamEvent::StepStart { step, .. } | LogStreamEvent::StepEnd { step, .. } => {
                Some(*step)
            }
            _ => None,
        })
        .collect();
    let step = match (
        steps.first().copied(),
        steps.iter().all(|s| Some(*s) == steps.first().copied()),
    ) {
        (Some(s), true) if s >= 0 => s,
        _ => -1,
    };
    // stream 标记：全部输出块一致才取值，否则空串（无/混合）。
    let streams: Vec<LogStream> = events
        .iter()
        .filter_map(|ev| match ev {
            LogStreamEvent::Output { stream, .. } => Some(*stream),
            _ => None,
        })
        .collect();
    let stream = match (
        streams.first().copied(),
        streams.iter().all(|s| Some(*s) == streams.first().copied()),
    ) {
        (Some(LogStream::Stdout), true) => "stdout",
        (Some(LogStream::Stderr), true) => "stderr",
        _ => "",
    };
    ChunkMeta {
        end_seq,
        step,
        stream,
    }
}

/// gzip 压缩（纯 Rust miniz_oxide 后端，ADR-0013）。
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("内存 Vec 写入恒可成功");
    encoder.finish().expect("内存 gzip 收尾恒可成功")
}

/// gzip 解压。
pub fn gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

impl LogStreamEvent {
    /// 本事件 seq（单一有序序列定位）。
    pub fn seq(&self) -> u64 {
        match self {
            Self::Output { seq, .. }
            | Self::StepStart { seq, .. }
            | Self::StepEnd { seq, .. }
            | Self::Truncated { seq, .. } => *seq,
        }
    }
}

/// 定位辅助：`(build_id, job_id, attempt)` 就地组 [`LogLocation`]。
pub fn location(build_id: i64, job_id: i64, attempt: i32) -> LogLocation {
    LogLocation {
        build_id,
        job_id,
        attempt,
    }
}

/// 整份日志的纯文本渲染（整份下载端点，text/plain）：输出块原样（含
/// ANSI，ADR-0013）、步骤开始回显 `$ <command>`、非零退出码与截断标注。
pub fn render_plain(events: &[LogStreamEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        match ev {
            LogStreamEvent::Output { text, .. } => out.push_str(text),
            LogStreamEvent::StepStart { command, .. } => {
                out.push_str("$ ");
                out.push_str(command);
                out.push('\n');
            }
            LogStreamEvent::StepEnd {
                exit_code: Some(code),
                ..
            } if *code != 0 => {
                out.push_str(&format!("exit code {code}\n"));
            }
            LogStreamEvent::StepEnd { .. } => {}
            LogStreamEvent::Truncated { limit_bytes, .. } => {
                out.push_str(&format!("[日志超限截断：上限 {limit_bytes} 字节]\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(seq: u64, stream: LogStream, text: &str) -> LogStreamEvent {
        LogStreamEvent::Output {
            seq,
            stream,
            text: text.into(),
        }
    }

    #[test]
    fn chunk_roundtrips_events_with_gzip() {
        let events = vec![
            output(0, LogStream::Stdout, "hello \x1b[32mworld\x1b[0m\n"),
            LogStreamEvent::StepStart {
                seq: 1,
                step: 0,
                name: String::new(),
                command: "cargo build".into(),
                started_at: 1000,
            },
            LogStreamEvent::StepEnd {
                seq: 2,
                step: 0,
                exit_code: Some(0),
                duration_ms: 250,
            },
            LogStreamEvent::Truncated {
                seq: 3,
                limit_bytes: DEFAULT_LOG_LIMIT_BYTES,
                dropped_bytes: 4096,
            },
        ];
        let chunk = encode_chunk(&events);
        assert_eq!(chunk.start_seq, 0);
        // 压缩确实生效（JSONL 明文更长）。
        let plain_len = gunzip(&chunk.compressed).unwrap().len();
        assert!(chunk.compressed.len() < plain_len, "gzip 应压缩");
        assert_eq!(decode_chunk(&chunk).unwrap(), events);
    }

    #[test]
    fn serde_shape_matches_frontend_contract() {
        // 与 sisyphus-web/src/api/sse.ts parseLogEvent 逐字对齐。
        let ev = output(5, LogStream::Stderr, "boom");
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            json,
            r#"{"type":"output","seq":5,"stream":"stderr","text":"boom"}"#
        );
        let ev = LogStreamEvent::StepStart {
            seq: 3,
            step: 0,
            name: String::new(),
            command: "cargo build".into(),
            started_at: 1000,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"step_start","seq":3,"step":0,"name":"","command":"cargo build","started_at":1000}"#
        );
        let ev = LogStreamEvent::StepEnd {
            seq: 4,
            step: 0,
            exit_code: None,
            duration_ms: 10,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"step_end","seq":4,"step":0,"exit_code":null,"duration_ms":10}"#
        );
        let ev = LogStreamEvent::Truncated {
            seq: 7,
            limit_bytes: 52428800,
            dropped_bytes: 1,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"truncated","seq":7,"limit_bytes":52428800,"dropped_bytes":1}"#
        );
        let job_end = JobEndEvent::new(8, "failed", Some(2));
        assert_eq!(
            serde_json::to_string(&job_end).unwrap(),
            r#"{"type":"job_end","seq":8,"status":"failed","exit_code":2}"#
        );
    }

    #[test]
    fn chunk_meta_derives_columns() {
        // 纯输出块：stream 一致、无步骤。
        let meta = chunk_meta(&[
            output(0, LogStream::Stdout, "a"),
            output(1, LogStream::Stdout, "b"),
        ]);
        assert_eq!(meta.end_seq, 1);
        assert_eq!(meta.step, -1);
        assert_eq!(meta.stream, "stdout");

        // 单步骤事件：step 取该序号。
        let meta = chunk_meta(&[
            LogStreamEvent::StepStart {
                seq: 2,
                step: 1,
                name: String::new(),
                command: "make".into(),
                started_at: 0,
            },
            output(3, LogStream::Stderr, "e"),
        ]);
        assert_eq!(meta.end_seq, 3);
        assert_eq!(meta.step, 1);
        assert_eq!(meta.stream, "stderr");

        // 混合 stream → 空串。
        let meta = chunk_meta(&[
            output(0, LogStream::Stdout, "a"),
            output(1, LogStream::Stderr, "b"),
        ]);
        assert_eq!(meta.stream, "");
    }

    #[test]
    fn render_plain_renders_output_steps_and_truncation() {
        let events = vec![
            LogStreamEvent::StepStart {
                seq: 0,
                step: 0,
                name: String::new(),
                command: "echo hi".into(),
                started_at: 0,
            },
            output(1, LogStream::Stdout, "hi\n"),
            LogStreamEvent::StepEnd {
                seq: 2,
                step: 0,
                exit_code: Some(0),
                duration_ms: 5,
            },
            LogStreamEvent::StepEnd {
                seq: 3,
                step: 1,
                exit_code: Some(3),
                duration_ms: 5,
            },
            LogStreamEvent::Truncated {
                seq: 4,
                limit_bytes: 1024,
                dropped_bytes: 9,
            },
        ];
        assert_eq!(
            render_plain(&events),
            "$ echo hi\nhi\nexit code 3\n[日志超限截断：上限 1024 字节]\n"
        );
    }

    #[test]
    fn decode_rejects_corrupt_line() {
        let bad = LogChunk {
            start_seq: 0,
            compressed: gzip(b"{\"type\":\"bogus\"}\n" as &[u8]),
        };
        assert!(matches!(decode_chunk(&bad), Err(LogCodecError::BadLine(_))));
    }
}

//! 任务 attempt 终态日志归档编码（ADR-0027）。
//!
//! 归档是一个带魔数的帧文件：每个帧独立 gzip 压缩一段 JSONL 事件，帧前
//! 带 little-endian 长度。独立帧使 Server 可以按稀疏索引 seek/decode，且
//! 单帧损坏不会牵连其它帧。归档只从 Agent 的持久 JSONL 缓冲生成，不会
//! 删除源缓冲；Server 确认 ready 后调用方才可清理。

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
use flate2::read::GzDecoder;
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::io::SeekFrom;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 归档格式版本魔数。
const MAGIC: &[u8; 8] = b"SYLOGA01";
/// 默认原始事件帧大小（约 4 MiB）。
const DEFAULT_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrameIndex {
    pub start_seq: u64,
    pub end_seq: u64,
    pub offset: u64,
    pub compressed_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ArchiveIndex {
    pub job_id: String,
    pub attempt: i32,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub sha256: String,
    pub frames: Vec<FrameIndex>,
}

#[derive(Debug, Clone)]
pub(crate) struct SealedArchive {
    pub path: PathBuf,
    pub index: ArchiveIndex,
}

#[derive(Debug)]
pub(crate) enum ArchiveError {
    Io(io::Error),
    Invalid(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "归档 IO 失败：{e}"),
            Self::Invalid(e) => write!(f, "归档格式非法：{e}"),
        }
    }
}
impl std::error::Error for ArchiveError {}
impl From<io::Error> for ArchiveError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// 将 JSONL 缓冲封存为归档及旁车索引。输入按行读取，要求 seq 连续单调。
pub(crate) fn seal(
    buffer_path: &Path,
    archive_path: &Path,
    index_path: &Path,
    job_id: &str,
    attempt: i32,
) -> Result<SealedArchive, ArchiveError> {
    seal_with_frame_bytes(
        buffer_path,
        archive_path,
        index_path,
        job_id,
        attempt,
        DEFAULT_FRAME_BYTES,
    )
}

fn seal_with_frame_bytes(
    buffer_path: &Path,
    archive_path: &Path,
    index_path: &Path,
    job_id: &str,
    attempt: i32,
    frame_bytes: usize,
) -> Result<SealedArchive, ArchiveError> {
    if frame_bytes == 0 {
        return Err(ArchiveError::Invalid("frame_bytes 不能为 0".into()));
    }
    if let Some(parent) = archive_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let input = File::open(buffer_path)?;
    let mut out = File::create(archive_path)?;
    out.write_all(MAGIC)?;
    let mut frame = Vec::new();
    let mut frames = Vec::new();
    let mut first_seq = None;
    let mut last_seq = None;
    let mut raw_bytes = 0u64;
    let mut expected_seq = 0u64;
    for line in BufReader::new(input).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| ArchiveError::Invalid(format!("JSONL 行无法解析：{e}")))?;
        let seq = value
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ArchiveError::Invalid("事件缺少 seq".into()))?;
        if seq != expected_seq {
            return Err(ArchiveError::Invalid(format!(
                "seq 不连续：期望 {expected_seq}，收到 {seq}"
            )));
        }
        expected_seq = expected_seq.saturating_add(1);
        first_seq.get_or_insert(seq);
        last_seq = Some(seq);
        let bytes = line.as_bytes();
        if !frame.is_empty() && frame.len().saturating_add(bytes.len() + 1) > frame_bytes {
            write_frame(&mut out, &mut frames, &frame)?;
            frame.clear();
        }
        frame.extend_from_slice(bytes);
        frame.push(b'\n');
        raw_bytes = raw_bytes.saturating_add((bytes.len() + 1) as u64);
    }
    if !frame.is_empty() {
        write_frame(&mut out, &mut frames, &frame)?;
    }
    out.sync_all()?;
    let compressed_bytes = out.metadata()?.len();
    let sha256 = sha256_path(archive_path)?;
    let index = ArchiveIndex {
        job_id: job_id.into(),
        attempt,
        first_seq,
        last_seq,
        raw_bytes,
        compressed_bytes,
        sha256,
        frames,
    };
    let data =
        serde_json::to_vec_pretty(&index).map_err(|e| ArchiveError::Invalid(e.to_string()))?;
    let mut idx = File::create(index_path)?;
    idx.write_all(&data)?;
    idx.sync_all()?;
    Ok(SealedArchive {
        path: archive_path.into(),
        index,
    })
}

fn write_frame(
    out: &mut File,
    frames: &mut Vec<FrameIndex>,
    raw: &[u8],
) -> Result<(), ArchiveError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(raw)?;
    let compressed = encoder.finish()?;
    let offset = out.stream_position()?.saturating_add(8);
    out.write_all(&(compressed.len() as u64).to_le_bytes())?;
    out.write_all(&compressed)?;
    let mut lines = raw.split(|b| *b == b'\n').filter(|l| !l.is_empty());
    let first = lines
        .next()
        .and_then(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    let last = raw
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .last()
        .and_then(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(first);
    frames.push(FrameIndex {
        start_seq: first,
        end_seq: last,
        offset,
        compressed_len: compressed.len() as u64,
    });
    Ok(())
}

#[cfg(test)]
fn read_frame(path: &Path, frame: &FrameIndex) -> Result<Vec<serde_json::Value>, ArchiveError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(frame.offset))?;
    let mut bytes = vec![0u8; frame.compressed_len as usize];
    file.read_exact(&mut bytes)?;
    let mut decoder = GzDecoder::new(bytes.as_slice());
    let mut raw = String::new();
    decoder.read_to_string(&mut raw)?;
    raw.lines()
        .map(|line| serde_json::from_str(line).map_err(|e| ArchiveError::Invalid(e.to_string())))
        .collect()
}

fn sha256_path(path: &Path) -> Result<String, ArchiveError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 归档传输缝；默认实现经 Server REST 上传，测试可注入 fake。
#[async_trait::async_trait]
pub(crate) trait LogArchiveIo: Send + Sync {
    async fn upload(
        &self,
        job_id: &str,
        attempt: i32,
        archive: &SealedArchive,
    ) -> Result<(), String>;
}

pub(crate) struct RealLogArchiveIo {
    client: reqwest::Client,
    api_url: Option<String>,
    token: Option<String>,
}

impl RealLogArchiveIo {
    pub(crate) fn new(api_url: Option<String>, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_url,
            token,
        }
    }
}

#[async_trait::async_trait]
impl LogArchiveIo for RealLogArchiveIo {
    async fn upload(
        &self,
        job_id: &str,
        attempt: i32,
        archive: &SealedArchive,
    ) -> Result<(), String> {
        let Some(base) = self.api_url.as_deref() else {
            return Err("api_url 未配置".into());
        };
        let url = format!(
            "{}/api/v1/agent/log-archives/{job_id}/{attempt}",
            base.trim_end_matches('/')
        );
        let file = tokio::fs::File::open(&archive.path)
            .await
            .map_err(|e| e.to_string())?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let mut request = self
            .client
            .post(url)
            .query(&[
                ("size", archive.index.compressed_bytes.to_string()),
                ("sha256", archive.index.sha256.clone()),
            ])
            .header(
                "x-sisyphus-archive-index",
                serde_json::to_string(&archive.index).map_err(|e| e.to_string())?,
            )
            .body(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("Server 拒绝日志归档：HTTP {}", response.status()))
        }
    }
}

/// 周期扫描非在途缓冲；进程重启后也会重新封存/上传，Server 确认后才清理。
pub(crate) fn spawn_retry_worker(
    logbuf: crate::logbuf::LogBuffer,
    in_flight: Arc<RwLock<Vec<String>>>,
    io: Arc<dyn LogArchiveIo>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let active = in_flight.read().await.clone();
            for (job_id, attempt) in logbuf.pending_archives() {
                if active.iter().any(|active_job| active_job == &job_id) {
                    continue;
                }
                let archive = match logbuf.seal_archive(&job_id, attempt) {
                    Ok(archive) => archive,
                    Err(error) => {
                        tracing::warn!(job = %job_id, attempt, error = %error, "待归档日志重新封存失败");
                        continue;
                    }
                };
                match io.upload(&job_id, attempt, &archive).await {
                    Ok(()) => {
                        logbuf.clear_deferred(&job_id, attempt);
                        tracing::info!(job = %job_id, attempt, "待归档日志后台重试成功");
                    }
                    Err(error) => {
                        tracing::warn!(job = %job_id, attempt, error = %error, "待归档日志后台重试失败");
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_independent_frames_with_sparse_index() {
        let dir = tempfile::tempdir().unwrap();
        let buf = dir.path().join("1-0.jsonl");
        std::fs::write(
            &buf,
            "{\"seq\":0,\"kind\":\"output\"}\n{\"seq\":1,\"kind\":\"step\"}\n",
        )
        .unwrap();
        let archive = seal_with_frame_bytes(
            &buf,
            &dir.path().join("a.slog"),
            &dir.path().join("a.json"),
            "1",
            0,
            1,
        )
        .unwrap();
        assert_eq!(archive.index.first_seq, Some(0));
        assert_eq!(archive.index.last_seq, Some(1));
        assert!(archive.index.frames.len() >= 2);
        let events = read_frame(&archive.path, &archive.index.frames[1]).unwrap();
        assert_eq!(events[0]["seq"], 1);
        assert_eq!(archive.index.sha256, sha256_path(&archive.path).unwrap());
    }

    #[test]
    fn rejects_seq_gap() {
        let dir = tempfile::tempdir().unwrap();
        let buf = dir.path().join("1-0.jsonl");
        std::fs::write(&buf, "{\"seq\":1,\"kind\":\"output\"}\n").unwrap();
        let err = seal(
            &buf,
            &dir.path().join("a.slog"),
            &dir.path().join("a.json"),
            "1",
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("seq 不连续"));
    }
}

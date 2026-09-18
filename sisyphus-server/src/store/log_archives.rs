//! Server 本地日志归档后端（ADR-0027 / #129）。

use std::path::{Path, PathBuf};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::StoreError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveFrameIndex {
    pub start_seq: u64,
    pub end_seq: u64,
    pub offset: u64,
    pub compressed_len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveIndex {
    pub job_id: String,
    pub attempt: i32,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub sha256: String,
    pub frames: Vec<ArchiveFrameIndex>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalLogArchiveStore {
    pool: SqlitePool,
    root: PathBuf,
}

impl LocalLogArchiveStore {
    pub(crate) fn new(pool: SqlitePool, root: PathBuf) -> Self {
        Self { pool, root }
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub fn final_path(&self, job_id: i64, attempt: i32) -> PathBuf {
        self.root.join(format!("{job_id}-{attempt}.slog"))
    }
    pub fn index_path(&self, job_id: i64, attempt: i32) -> PathBuf {
        self.root.join(format!("{job_id}-{attempt}.json"))
    }

    /// 在接收正文前登记待归档；执行结果与本状态分属不同表、互不覆盖。
    pub async fn mark_pending(&self, job_id: i64, attempt: i32) -> Result<(), StoreError> {
        let now = crate::store::now_ms();
        let path = self.final_path(job_id, attempt);
        let index_path = self.index_path(job_id, attempt);
        sqlx::query(
            "INSERT INTO log_archives (job_id, attempt, state, path, index_path, created_at)
             VALUES (?, ?, 'pending', ?, ?, ?)
             ON CONFLICT(job_id, attempt) DO UPDATE SET state = CASE WHEN log_archives.state IN ('ready', 'lost') THEN log_archives.state ELSE 'pending' END",
        )
        .bind(job_id)
        .bind(attempt)
        .bind(path.to_string_lossy().as_ref())
        .bind(index_path.to_string_lossy().as_ref())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 校验临时正文大小和 SHA-256，写入索引后原子改名，最后登记 ready。
    /// 在数据库确认前不会删除/覆盖已有 ready 正文。
    pub async fn publish(
        &self,
        job_id: i64,
        attempt: i32,
        temp_path: &Path,
        expected_size: u64,
        expected_sha256: &str,
        index: &ArchiveIndex,
    ) -> Result<(), StoreError> {
        let meta = tokio::fs::metadata(temp_path).await?;
        if meta.len() != expected_size {
            return Err(StoreError::Conflict("日志归档大小校验失败".into()));
        }
        if !is_sha256(expected_sha256) {
            return Err(StoreError::Invalid("日志归档 SHA-256 非法".into()));
        }
        let got = sha256_file(temp_path).await?;
        if got != expected_sha256 {
            return Err(StoreError::Conflict("日志归档 SHA-256 校验失败".into()));
        }
        let mut header_file = tokio::fs::File::open(temp_path).await?;
        let mut magic = [0u8; 8];
        header_file.read_exact(&mut magic).await?;
        if &magic != b"SYLOGA01" {
            return Err(StoreError::Invalid("日志归档魔数或版本非法".into()));
        }
        validate_index(index, expected_size, expected_sha256)?;
        if index.job_id != job_id.to_string() || index.attempt != attempt {
            return Err(StoreError::Invalid("日志归档索引归属不匹配".into()));
        }
        tokio::fs::create_dir_all(&self.root).await?;
        let final_path = self.final_path(job_id, attempt);
        let index_path = self.index_path(job_id, attempt);
        if let Some((state, size, sha256)) = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT state, size, sha256 FROM log_archives WHERE job_id = ? AND attempt = ?",
        )
        .bind(job_id)
        .bind(attempt)
        .fetch_optional(&self.pool)
        .await?
        {
            if state == "lost" {
                return Err(StoreError::Conflict("日志归档已过期并标记为 lost".into()));
            }
            if state == "ready" {
                if size as u64 != expected_size || sha256 != expected_sha256 {
                    return Err(StoreError::Conflict("ready 日志归档内容不可改写".into()));
                }
                tokio::fs::remove_file(temp_path).await?;
                return Ok(());
            }
        }
        tokio::fs::rename(temp_path, &final_path).await?;
        let index_data =
            serde_json::to_vec(index).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let index_tmp = index_path.with_extension("json.tmp");
        tokio::fs::write(&index_tmp, index_data).await?;
        tokio::fs::rename(&index_tmp, &index_path).await?;
        let now = crate::store::now_ms();
        sqlx::query(
            "INSERT INTO log_archives (job_id, attempt, state, path, index_path, size, sha256, first_seq, last_seq, created_at, ready_at)
             VALUES (?, ?, 'ready', ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(job_id, attempt) DO UPDATE SET state=CASE WHEN log_archives.state='lost' THEN 'lost' ELSE 'ready' END, path=excluded.path, index_path=excluded.index_path,
             size=excluded.size, sha256=excluded.sha256, first_seq=excluded.first_seq, last_seq=excluded.last_seq, ready_at=excluded.ready_at",
        )
        .bind(job_id).bind(attempt).bind(final_path.to_string_lossy().as_ref()).bind(index_path.to_string_lossy().as_ref())
        .bind(expected_size as i64).bind(expected_sha256)
        .bind(index.first_seq.map(|v| v as i64)).bind(index.last_seq.map(|v| v as i64)).bind(now).bind(now)
        .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn load_index(
        &self,
        job_id: i64,
        attempt: i32,
    ) -> Result<Option<ArchiveIndex>, StoreError> {
        let Some((state, path)) = sqlx::query_as::<_, (String, String)>(
            "SELECT state, index_path FROM log_archives WHERE job_id = ? AND attempt = ?",
        )
        .bind(job_id)
        .bind(attempt)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        if state != "ready" {
            return Ok(None);
        }
        let data = tokio::fs::read(path).await?;
        serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| StoreError::Invalid(e.to_string()))
    }

    /// 读取从游标开始的下一帧；调用方逐帧轮询，避免把整份归档拼入内存。
    pub async fn read_events(
        &self,
        job_id: i64,
        attempt: i32,
        from_seq: u64,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let Some(index) = self.load_index(job_id, attempt).await? else {
            return Ok(Vec::new());
        };
        let path = self.final_path(job_id, attempt);
        for frame in index.frames.into_iter().filter(|f| f.end_seq >= from_seq) {
            let mut values = Vec::new();
            for value in read_frame_values(&path, &frame).await? {
                if value
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|seq| seq >= from_seq)
                {
                    values.push(value);
                }
            }
            return Ok(values);
        }
        Ok(Vec::new())
    }

    /// 逐帧解码并渲染纯文本；任一时刻只在内存中保留一个约 4 MiB 原始帧。
    pub async fn stream_plain(
        &self,
        job_id: i64,
        attempt: i32,
    ) -> Result<Option<super::ByteStream>, StoreError> {
        let Some(index) = self.load_index(job_id, attempt).await? else {
            return Ok(None);
        };
        let path = self.final_path(job_id, attempt);
        let stream = futures::stream::iter(index.frames).then(move |frame| {
            let path = path.clone();
            async move {
                let values = read_frame_values(&path, &frame)
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let events = values
                    .into_iter()
                    .filter_map(agent_json_event)
                    .collect::<Vec<_>>();
                Ok(crate::logs::render_plain(&events).into_bytes())
            }
        });
        Ok(Some(Box::pin(stream)))
    }
}

async fn read_frame_values(
    path: &Path,
    frame: &ArchiveFrameIndex,
) -> Result<Vec<serde_json::Value>, StoreError> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(frame.offset)).await?;
    let mut compressed = vec![0; frame.compressed_len as usize];
    file.read_exact(&mut compressed).await?;
    let raw = tokio::task::spawn_blocking(move || {
        let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
        let mut text = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut text).map(|_| text)
    })
    .await
    .map_err(|e| StoreError::Invalid(e.to_string()))??;
    raw.lines()
        .map(|line| serde_json::from_str(line).map_err(|e| StoreError::Invalid(e.to_string())))
        .collect()
}

pub(crate) fn agent_json_event(value: serde_json::Value) -> Option<crate::logs::LogStreamEvent> {
    let seq = value.get("seq")?.as_u64()?;
    match value.get("kind")?.as_str()? {
        "output" => {
            use base64ct::Encoding;
            let stream = match value.get("stream")?.as_i64()? {
                1 => crate::logs::LogStream::Stderr,
                _ => crate::logs::LogStream::Stdout,
            };
            let data =
                base64ct::Base64UrlUnpadded::decode_vec(value.get("data")?.as_str()?).ok()?;
            Some(crate::logs::LogStreamEvent::Output {
                seq,
                stream,
                text: String::from_utf8_lossy(&data).into_owned(),
            })
        }
        "step" => {
            let step = value.get("step_seq")?.as_i64()? as i32;
            let started = value.get("started_at")?.as_i64()?;
            let ended = value.get("ended_at")?.as_i64()?;
            let command = value.get("command")?.as_str()?.to_string();
            let exit_code = value
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map(|v| v as i32);
            if ended != 0 {
                Some(crate::logs::LogStreamEvent::StepEnd {
                    seq,
                    step,
                    exit_code,
                    duration_ms: ended.saturating_sub(started),
                })
            } else {
                Some(crate::logs::LogStreamEvent::StepStart {
                    seq,
                    step,
                    name: String::new(),
                    command,
                    started_at: started,
                })
            }
        }
        "truncated" => Some(crate::logs::LogStreamEvent::Truncated {
            seq,
            dropped_bytes: value.get("dropped_bytes")?.as_u64()?,
            limit_bytes: crate::logs::DEFAULT_LOG_LIMIT_BYTES,
        }),
        _ => None,
    }
}

async fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 128 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn validate_index(index: &ArchiveIndex, size: u64, sha256: &str) -> Result<(), StoreError> {
    if index.compressed_bytes != size || index.sha256 != sha256 {
        return Err(StoreError::Conflict("日志归档索引摘要或大小不匹配".into()));
    }
    let mut next_seq = index.first_seq.unwrap_or(0);
    let mut previous_end = 8u64;
    for frame in &index.frames {
        if frame.start_seq != next_seq || frame.end_seq < frame.start_seq {
            return Err(StoreError::Invalid("日志归档帧 seq 范围不连续".into()));
        }
        let frame_end = frame
            .offset
            .checked_add(frame.compressed_len)
            .ok_or_else(|| StoreError::Invalid("日志归档帧范围溢出".into()))?;
        if frame.offset < previous_end || frame_end > size {
            return Err(StoreError::Invalid("日志归档帧字节范围非法".into()));
        }
        previous_end = frame_end;
        next_seq = frame.end_seq.saturating_add(1);
    }
    if index.frames.last().map(|f| f.end_seq) != index.last_seq {
        return Err(StoreError::Invalid("日志归档末尾 seq 与索引不一致".into()));
    }
    Ok(())
}

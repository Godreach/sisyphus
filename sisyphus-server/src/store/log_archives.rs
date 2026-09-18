//! Server 本地日志归档后端（ADR-0027 / #129）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::storage::{ObjectClass, ObjectPhase, S3Client, object_key};

use super::StoreError;

/// 日志状态与最后可用信息，不暴露正文路径或签名 URL。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema, sqlx::FromRow)]
pub struct ArchiveStatus {
    /// 任务行。
    pub job_id: i64,
    /// 执行轮次。
    pub attempt: i32,
    /// pending / ready / lost。
    pub state: String,
    /// 后端。
    pub backend: String,
    /// 最后声明的归档大小。
    pub size: i64,
    /// 最后可用序号。
    pub last_seq: Option<i64>,
    /// 固定执行结束时刻。
    pub execution_finished_at: Option<i64>,
    /// 丢失或清理原因。
    pub lost_reason: Option<String>,
    /// 标记时刻。
    pub lost_at: Option<i64>,
    /// 负责的 Agent。
    pub agent_name: Option<String>,
    /// Agent 最后心跳。
    pub last_seen_at: Option<i64>,
    /// 任务名。
    pub job_name: String,
    /// 构建号。
    pub build_number: i64,
    /// Pipeline。
    pub pipeline_name: String,
}

const STATUS_QUERY: &str = "SELECT a.job_id, a.attempt, a.state, a.backend, a.size, a.last_seq,
    a.execution_finished_at, a.lost_reason, a.lost_at, g.name AS agent_name, g.last_seen_at,
    j.name AS job_name, b.number AS build_number, b.pipeline_name
    FROM log_archives a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id
    LEFT JOIN agents g ON g.id=j.agent_id";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveFrameIndex {
    pub start_seq: u64,
    pub end_seq: u64,
    pub offset: u64,
    pub compressed_len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveIndex {
    #[serde(default)]
    pub execution_finished_at_ms: Option<i64>,
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
    s3: Option<Arc<S3Client>>,
}

impl LocalLogArchiveStore {
    pub async fn status(
        &self,
        job: i64,
        attempt: i32,
    ) -> Result<Option<ArchiveStatus>, StoreError> {
        Ok(sqlx::QueryBuilder::<sqlx::Sqlite>::new(STATUS_QUERY)
            .push(" WHERE a.job_id=")
            .push_bind(job)
            .push(" AND a.attempt=")
            .push_bind(attempt)
            .build_query_as()
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn backlog(&self, agent: Option<&str>) -> Result<Vec<ArchiveStatus>, StoreError> {
        Ok(sqlx::QueryBuilder::<sqlx::Sqlite>::new(STATUS_QUERY)
            .push(" WHERE a.state IN ('pending', 'lost') AND (")
            .push_bind(agent)
            .push(" IS NULL OR g.name=")
            .push_bind(agent)
            .push(") ORDER BY a.created_at, a.job_id LIMIT 500")
            .build_query_as()
            .fetch_all(&self.pool)
            .await?)
    }

    /// 丢失标记与审计同事务提交；不改执行结果、不误伤 ready。
    pub async fn mark_lost(
        &self,
        job: i64,
        attempt: i32,
        reason: &str,
        actor: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let now = super::now_ms();
        let changed = sqlx::query("UPDATE log_archives SET state='lost', lost_reason=?, lost_at=?
            WHERE job_id=? AND attempt=? AND state='pending'
            AND EXISTS (SELECT 1 FROM jobs WHERE id=job_id AND status NOT IN ('queued', 'running', 'unknown'))")
            .bind(reason).bind(now).bind(job).bind(attempt).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(StoreError::Conflict(
                "只有待归档日志可以标记永久丢失".into(),
            ));
        }
        let detail =
            serde_json::json!({"job_id":job,"attempt":attempt,"reason":reason}).to_string();
        sqlx::query("INSERT INTO audit_log (ts, actor, event_type, project_name, detail) VALUES (?, ?, 'log_archive_lost', NULL, ?)")
            .bind(now).bind(actor).bind(detail).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) fn new(pool: SqlitePool, root: PathBuf) -> Self {
        Self {
            pool,
            root,
            s3: None,
        }
    }
    pub(crate) fn with_s3(mut self, s3: Option<Arc<S3Client>>) -> Self {
        self.s3 = s3;
        self
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

    /// 执行结束的固定时钟锚点；重复报告、归档补传不能延长保留期。
    pub async fn record_execution_end(
        &self,
        job_id: i64,
        attempt: i32,
        ended: Option<i64>,
    ) -> Result<(), StoreError> {
        self.mark_pending(job_id, attempt).await?;
        let now = crate::store::now_ms();
        let ended = ended.filter(|value| *value > 0).unwrap_or(now).min(now);
        sqlx::query(
            "UPDATE log_archives SET execution_finished_at =
            CASE WHEN execution_finished_at IS NULL THEN ? ELSE MIN(execution_finished_at, ?) END
            WHERE job_id=? AND attempt=?",
        )
        .bind(ended)
        .bind(ended)
        .bind(job_id)
        .bind(attempt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 为 S3 直传登记只可写的临时 key；重试复用待归档许可，ready 不再签发 PUT。
    pub async fn grant_s3(
        &self,
        job_id: i64,
        attempt: i32,
        prefix: &str,
        index: &ArchiveIndex,
    ) -> Result<Option<String>, StoreError> {
        validate_index(index, index.compressed_bytes, &index.sha256)?;
        self.record_execution_end(job_id, attempt, index.execution_finished_at_ms)
            .await?;
        if index.compressed_bytes > i64::MAX as u64 {
            return Err(StoreError::Invalid("日志归档大小超出存储范围".into()));
        }
        if index.job_id != job_id.to_string()
            || index.attempt != attempt
            || !is_sha256(&index.sha256)
        {
            return Err(StoreError::Invalid("日志归档索引归属或摘要非法".into()));
        }
        let final_key = object_key(
            prefix,
            ObjectClass::Logs,
            ObjectPhase::Final,
            &format!("{job_id}/{attempt}.slog"),
        );
        let existing = sqlx::query_as::<_, (String, String, i64, String, Option<String>, Option<String>)>(
            "SELECT state, backend, size, sha256, temp_path, index_json FROM log_archives WHERE job_id = ? AND attempt = ?",
        )
        .bind(job_id).bind(attempt).fetch_optional(&self.pool).await?;
        if let Some((state, backend, size, sha, temp, old_index)) = existing {
            let index_matches = old_index
                .as_deref()
                .and_then(|raw| serde_json::from_str::<ArchiveIndex>(raw).ok())
                .is_some_and(|old| old == *index);
            if state == "lost"
                || state == "ready"
                    && (backend != "s3"
                        || size as u64 != index.compressed_bytes
                        || sha != index.sha256
                        || !index_matches)
            {
                return Err(StoreError::Conflict(
                    "日志归档已固化或丢失，不可改写".into(),
                ));
            }
            if state == "ready" {
                return Ok(None);
            }
            if backend == "s3" {
                if size as u64 != index.compressed_bytes || sha != index.sha256 || !index_matches {
                    return Err(StoreError::Conflict("待归档日志摘要不可变更".into()));
                }
                if let Some(temp) = temp {
                    return Ok(Some(temp));
                }
            }
        }
        let temp_key = object_key(
            prefix,
            ObjectClass::Logs,
            ObjectPhase::Temporary,
            &format!("{job_id}/{attempt}/{}.slog", random_nonce()),
        );
        let index_json =
            serde_json::to_string(index).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let now = crate::store::now_ms();
        sqlx::query(
            "INSERT INTO log_archives (job_id, attempt, state, path, index_path, size, sha256, first_seq, last_seq, created_at, backend, index_json, temp_path)
             VALUES (?, ?, 'pending', ?, '', ?, ?, ?, ?, ?, 's3', ?, ?)
             ON CONFLICT(job_id, attempt) DO UPDATE SET backend='s3', path=excluded.path, index_path='', size=excluded.size,
             sha256=excluded.sha256, first_seq=excluded.first_seq, last_seq=excluded.last_seq,
             index_json=excluded.index_json, temp_path=excluded.temp_path WHERE log_archives.state='pending'",
        )
        .bind(job_id).bind(attempt).bind(&final_key).bind(index.compressed_bytes as i64).bind(&index.sha256)
        .bind(index.first_seq.map(|v| v as i64)).bind(index.last_seq.map(|v| v as i64))
        .bind(now).bind(index_json).bind(&temp_key).execute(&self.pool).await?;
        Ok(Some(temp_key))
    }

    /// 完成直传：Server 流式校验临时对象与最终对象，最终 key 永不签发写 URL。
    pub async fn publish_s3(
        &self,
        job_id: i64,
        attempt: i32,
        copy_limit: u64,
        copy_part_size: u64,
    ) -> Result<(), StoreError> {
        let s3 = self
            .s3
            .as_ref()
            .ok_or_else(|| StoreError::Conflict("S3 日志后端未配置".into()))?;
        let row = sqlx::query_as::<_, (String, String, String, i64, String, Option<String>, Option<String>)>(
            "SELECT state, backend, path, size, sha256, index_json, temp_path FROM log_archives WHERE job_id=? AND attempt=?",
        ).bind(job_id).bind(attempt).fetch_optional(&self.pool).await?
            .ok_or_else(|| StoreError::NotFound("日志归档上传许可不存在".into()))?;
        let (state, backend, final_key, size, sha, index_json, temp_key) = row;
        if backend != "s3" || state == "lost" {
            return Err(StoreError::Conflict("日志归档后端或状态不匹配".into()));
        }
        if state == "ready" {
            return Ok(());
        }
        let temp_key =
            temp_key.ok_or_else(|| StoreError::Invalid("日志归档临时 key 缺失".into()))?;
        let index: ArchiveIndex = serde_json::from_str(index_json.as_deref().unwrap_or(""))
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        validate_index(&index, size as u64, &sha)?;
        let magic = s3.get_range(&temp_key, 0, 7).await.map_err(s3_io)?;
        if magic != b"SYLOGA01" {
            return Err(StoreError::Invalid("日志归档魔数或版本非法".into()));
        }
        let (actual_size, digest) = s3.hash_object(&temp_key).await.map_err(s3_io)?;
        if actual_size != size as u64 || digest != sha {
            return Err(StoreError::Conflict(
                "日志归档大小或 SHA-256 校验失败".into(),
            ));
        }
        // 每次完成使用独立的最终 key；只有条件更新获胜者才能把它暴露为 ready。
        // 旧 PUT URL 可以改写临时对象，但并发/迟到的完成请求不得覆盖或删除获胜者。
        let candidate_key = format!("{final_key}.{}", random_nonce());
        let registered = sqlx::query(
            "INSERT INTO log_archive_publish_candidates (key, job_id, attempt, created_at)
             SELECT ?, job_id, attempt, ? FROM log_archives
             WHERE job_id=? AND attempt=? AND state='pending' AND backend='s3' AND path=? AND temp_path=?",
        )
        .bind(&candidate_key)
        .bind(crate::store::now_ms())
        .bind(job_id)
        .bind(attempt)
        .bind(&final_key)
        .bind(&temp_key)
        .execute(&self.pool)
        .await?;
        if registered.rows_affected() != 1 {
            return Err(StoreError::Conflict(
                "日志归档状态已变化，未开始复制".into(),
            ));
        }
        let mut copy_completed = false;
        let publish_result = async {
            s3.copy_object_adaptive(
                &temp_key,
                &candidate_key,
                actual_size,
                copy_limit,
                copy_part_size,
            )
            .await
            .map_err(s3_io)?;
            copy_completed = true;
            let (final_size, final_digest) =
                s3.hash_object(&candidate_key).await.map_err(s3_io)?;
            if final_size != actual_size || final_digest != sha {
                return Err(StoreError::Conflict("最终日志归档校验失败".into()));
            }
            let now = crate::store::now_ms();
            let updated = sqlx::query("UPDATE log_archives SET state='ready', path=?, ready_at=? WHERE job_id=? AND attempt=? AND state='pending' AND backend='s3' AND path=? AND temp_path=?")
                .bind(&candidate_key).bind(now).bind(job_id).bind(attempt).bind(&final_key).bind(&temp_key)
                .execute(&self.pool).await?;
            if updated.rows_affected() != 1 {
                return Err(StoreError::Conflict(
                    "日志归档状态已变化，未确认 ready".into(),
                ));
            }
            Ok(())
        }
        .await;
        if let Err(error) = publish_result {
            match s3.delete_object(&candidate_key).await {
                Ok(()) => {
                    // 超时不等于 S3 停止复制；结果不确定时保留 key 供定期清理。
                    if copy_completed
                        && let Err(cleanup_error) =
                            sqlx::query("DELETE FROM log_archive_publish_candidates WHERE key=?")
                                .bind(&candidate_key)
                                .execute(&self.pool)
                                .await
                    {
                        tracing::warn!(job_id, attempt, error = %cleanup_error, "候选对象已删除，但登记清理失败");
                    }
                }
                Err(cleanup_error) => {
                    tracing::warn!(job_id, attempt, error = %cleanup_error, "失败的日志归档候选对象清理失败，保留登记待清理");
                }
            }
            return Err(error);
        }
        if let Err(error) = sqlx::query("DELETE FROM log_archive_publish_candidates WHERE key=?")
            .bind(&candidate_key)
            .execute(&self.pool)
            .await
        {
            tracing::warn!(job_id, attempt, error = %error, "ready 候选对象登记清理失败，保留至归档清理");
        }
        match s3.delete_object(&temp_key).await {
            Ok(()) => {
                sqlx::query("UPDATE log_archives SET temp_path=NULL WHERE job_id=? AND attempt=? AND state='ready' AND temp_path=?")
                    .bind(job_id).bind(attempt).bind(&temp_key).execute(&self.pool).await?;
            }
            Err(error) => {
                tracing::warn!(job_id, attempt, error = %error, "日志归档临时对象清理失败，留待保留清理")
            }
        }
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
        // 与丢失/清理动作串行：先取得 SQLite 写锁，避免清理之后迟到 rename。
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE log_archives SET state=state WHERE job_id=? AND attempt=?")
            .bind(job_id)
            .bind(attempt)
            .execute(&mut *tx)
            .await?;
        if let Some((state, size, sha256)) = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT state, size, sha256 FROM log_archives WHERE job_id = ? AND attempt = ?",
        )
        .bind(job_id)
        .bind(attempt)
        .fetch_optional(&mut *tx)
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
             size=excluded.size, sha256=excluded.sha256, first_seq=excluded.first_seq, last_seq=excluded.last_seq, ready_at=excluded.ready_at,
             backend='local', index_json=NULL, temp_path=NULL",
        )
        .bind(job_id).bind(attempt).bind(final_path.to_string_lossy().as_ref()).bind(index_path.to_string_lossy().as_ref())
        .bind(expected_size as i64).bind(expected_sha256)
        .bind(index.first_seq.map(|v| v as i64)).bind(index.last_seq.map(|v| v as i64)).bind(now).bind(now)
        .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn load_index(
        &self,
        job_id: i64,
        attempt: i32,
    ) -> Result<Option<ArchiveIndex>, StoreError> {
        let Some((state, backend, path, index_json, size, sha)) = sqlx::query_as::<_, (String, String, String, Option<String>, i64, String)>(
            "SELECT state, backend, index_path, index_json, size, sha256 FROM log_archives WHERE job_id = ? AND attempt = ?",
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
        let data = if backend == "s3" {
            index_json
                .ok_or_else(|| StoreError::Invalid("S3 日志归档索引缺失".into()))?
                .into_bytes()
        } else {
            tokio::fs::read(path).await?
        };
        let index: ArchiveIndex =
            serde_json::from_slice(&data).map_err(|e| StoreError::Invalid(e.to_string()))?;
        validate_index(&index, size as u64, &sha)?;
        Ok(Some(index))
    }

    /// 读取从游标开始的下一帧；调用方逐帧轮询，避免把整份归档拼入内存。
    pub async fn read_events(
        &self,
        job_id: i64,
        attempt: i32,
        from_seq: u64,
    ) -> Result<Option<Vec<serde_json::Value>>, StoreError> {
        let Some(index) = self.load_index(job_id, attempt).await? else {
            return Ok(None);
        };
        let (backend, path) = self.ready_location(job_id, attempt).await?;
        if let Some(frame) = index.frames.into_iter().find(|f| f.end_seq >= from_seq) {
            let mut values = Vec::new();
            for value in self.frame_values(&backend, &path, &frame).await? {
                if value
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|seq| seq >= from_seq)
                {
                    values.push(value);
                }
            }
            return Ok(Some(values));
        }
        Ok(Some(Vec::new()))
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
        let (backend, path) = self.ready_location(job_id, attempt).await?;
        let archive = self.clone();
        let stream = futures::stream::iter(index.frames).then(move |frame| {
            let path = path.clone();
            let backend = backend.clone();
            let archive = archive.clone();
            async move {
                let values = archive
                    .frame_values(&backend, &path, &frame)
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

    async fn ready_location(
        &self,
        job_id: i64,
        attempt: i32,
    ) -> Result<(String, String), StoreError> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT backend, path FROM log_archives WHERE job_id=? AND attempt=? AND state='ready'",
        )
        .bind(job_id)
        .bind(attempt)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound("ready 日志归档不存在".into()))
    }

    async fn frame_values(
        &self,
        backend: &str,
        path: &str,
        frame: &ArchiveFrameIndex,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        if backend == "s3" {
            let s3 = self
                .s3
                .as_ref()
                .ok_or_else(|| StoreError::Invalid("S3 日志后端未配置".into()))?;
            let end = frame.offset + frame.compressed_len - 1;
            let bytes = s3.get_range(path, frame.offset, end).await.map_err(s3_io)?;
            decode_frame_values(bytes).await
        } else {
            read_frame_values(Path::new(path), frame).await
        }
    }
}

fn random_nonce() -> String {
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    nonce.iter().map(|b| format!("{b:02x}")).collect()
}

fn s3_io(error: crate::storage::StorageError) -> StoreError {
    StoreError::Io(std::io::Error::other(error))
}

async fn read_frame_values(
    path: &Path,
    frame: &ArchiveFrameIndex,
) -> Result<Vec<serde_json::Value>, StoreError> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(frame.offset)).await?;
    let mut compressed = vec![0; frame.compressed_len as usize];
    file.read_exact(&mut compressed).await?;
    decode_frame_values(compressed).await
}

async fn decode_frame_values(compressed: Vec<u8>) -> Result<Vec<serde_json::Value>, StoreError> {
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
        if frame.compressed_len == 0 || frame.compressed_len > 16 * 1024 * 1024 {
            return Err(StoreError::Invalid("日志归档帧大小非法".into()));
        }
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

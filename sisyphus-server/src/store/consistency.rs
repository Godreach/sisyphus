//! 只读的 SQLite / 本地对象 / S3 一致性检查（#133）。
//!
//! 检查不会修复、删除或发布任何对象。默认只做 HEAD/stat 大小核对；调用方
//! 显式传入 `deep_hash` 时才逐对象计算 SHA-256。

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use super::StoreError;
use crate::storage::{
    ObjectClass, ObjectPhase, S3Client, StorageError, artifact_blob_name, object_key,
};

#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConsistencyFinding {
    /// ready_missing_object / size_mismatch / hash_mismatch / unregistered_object。
    pub kind: String,
    /// artifact 或 log_archive。
    pub resource: String,
    pub backend: String,
    pub key: String,
    pub expected_size: Option<u64>,
    pub actual_size: Option<u64>,
    pub detail: Option<String>,
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConsistencyBacklog {
    /// 尚未发布的单对象/产物集元数据。
    pub pending_uploads: u64,
    /// 尚未完成的 multipart 会话。
    pub pending_multipart_uploads: u64,
    /// 异步删除队列（queued/running/failed）。
    pub pending_deletions: u64,
    /// 待归档日志（Agent 仍需重试）。
    pub pending_archives: u64,
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConsistencyReport {
    pub checked_at: i64,
    pub deep_hash: bool,
    pub backend: String,
    pub findings: Vec<ConsistencyFinding>,
    pub backlog: ConsistencyBacklog,
    /// S3 暂时不可达等非破坏性检查错误；存在时不把对象误报成缺失。
    pub errors: Vec<String>,
}

impl ConsistencyReport {
    /// 最近一次检查发现的问题数。
    pub fn issue_count(&self) -> u64 {
        self.findings.len() as u64
    }
}

/// 执行一次完整的只读检查。
pub async fn run(
    pool: &SqlitePool,
    local_artifacts_root: &Path,
    s3: Option<&S3Client>,
    deep_hash: bool,
) -> Result<ConsistencyReport, StoreError> {
    let mut findings = Vec::new();
    let mut errors = Vec::new();
    let mut expected_s3 = HashSet::new();

    let artifact_rows: Vec<(i64, String, i64, String, String, String)> = sqlx::query_as(
        "SELECT id, path, size, sha256, backend, state FROM artifacts WHERE state = 'ready'",
    )
    .fetch_all(pool)
    .await?;
    for (_id, path, size, sha256, backend, _state) in artifact_rows {
        check_object(
            &mut findings,
            &mut errors,
            local_artifacts_root,
            s3,
            "artifact",
            &backend,
            &path,
            size,
            &sha256,
            deep_hash,
        )
        .await;
        if backend == "s3" {
            expected_s3.insert(path);
        }
    }

    let log_rows: Vec<(String, String, i64, String, String, String)> = sqlx::query_as(
        "SELECT path, index_path, size, sha256, backend, state FROM log_archives WHERE state = 'ready'",
    )
    .fetch_all(pool)
    .await?;
    for (path, index_path, size, sha256, backend, _state) in log_rows {
        check_object(
            &mut findings,
            &mut errors,
            local_artifacts_root,
            s3,
            "log_archive",
            &backend,
            &path,
            size,
            &sha256,
            deep_hash,
        )
        .await;
        if backend == "s3" {
            expected_s3.insert(path);
            if !index_path.is_empty() {
                expected_s3.insert(index_path);
            }
        } else if !index_path.is_empty() {
            check_local_index(
                &mut findings,
                &mut errors,
                local_artifacts_root,
                &index_path,
            )
            .await;
        }
    }

    // 临时对象与候选最终对象都有合法的数据库登记，不要误报为 bucket 漂移。
    for key in sqlx::query_scalar::<_, String>(
        "SELECT path FROM artifacts WHERE backend='s3' AND state != 'ready'
         UNION SELECT temp_path FROM log_archives WHERE backend='s3' AND temp_path IS NOT NULL
         UNION SELECT key FROM log_archive_publish_candidates",
    )
    .fetch_all(pool)
    .await?
    {
        expected_s3.insert(key);
    }
    for key in sqlx::query_scalar::<_, String>("SELECT object_key FROM artifact_multipart_uploads")
        .fetch_all(pool)
        .await?
    {
        expected_s3.insert(key);
    }

    if let Some(s3) = s3 {
        let pending: Vec<(i64, Option<i64>, Option<i32>, String)> = sqlx::query_as(
            "SELECT build_id, job_id, attempt, name FROM artifacts
             WHERE backend='s3' AND state='pending'",
        )
        .fetch_all(pool)
        .await?;
        for (build_id, job_id, attempt, name) in pending {
            if let (Some(job_id), Some(attempt)) = (job_id, attempt) {
                let blob = artifact_blob_name(build_id, job_id, attempt, &name);
                expected_s3.insert(object_key(
                    s3.prefix(),
                    ObjectClass::Artifacts,
                    ObjectPhase::Temporary,
                    &blob,
                ));
                expected_s3.insert(object_key(
                    s3.prefix(),
                    ObjectClass::Artifacts,
                    ObjectPhase::Final,
                    &blob,
                ));
            }
        }
        match s3.list_objects(s3.prefix()).await {
            Ok(objects) => {
                for object in objects {
                    if !expected_s3.contains(&object.key) {
                        let resource = if object.key.contains("/logs/") {
                            "log_archive"
                        } else {
                            "artifact"
                        };
                        findings.push(ConsistencyFinding {
                            kind: "unregistered_object".into(),
                            resource: resource.into(),
                            backend: "s3".into(),
                            key: object.key,
                            expected_size: None,
                            actual_size: Some(object.size),
                            detail: Some("对象未在 SQLite 目录登记".into()),
                        });
                    }
                }
            }
            Err(error) => errors.push(format!("S3 列举对象失败：{error}")),
        }
    }

    let backlog = ConsistencyBacklog {
        pending_uploads: scalar(pool, "SELECT COUNT(*) FROM artifacts WHERE state='pending'")
            .await? as u64,
        pending_multipart_uploads: scalar(pool, "SELECT COUNT(*) FROM artifact_multipart_uploads")
            .await? as u64,
        pending_deletions: scalar(
            pool,
            "SELECT COUNT(*) FROM artifact_deletions WHERE state != 'completed'",
        )
        .await? as u64,
        pending_archives: scalar(
            pool,
            "SELECT COUNT(*) FROM log_archives WHERE state='pending'",
        )
        .await? as u64,
    };
    let backend = if s3.is_some() { "s3" } else { "local" };
    Ok(ConsistencyReport {
        checked_at: super::now_ms(),
        deep_hash,
        backend: backend.into(),
        findings,
        backlog,
        errors,
    })
}

async fn scalar(pool: &SqlitePool, query: &'static str) -> Result<i64, StoreError> {
    Ok(sqlx::query_scalar(query).fetch_one(pool).await?)
}

async fn check_object(
    findings: &mut Vec<ConsistencyFinding>,
    errors: &mut Vec<String>,
    local_root: &Path,
    s3: Option<&S3Client>,
    resource: &str,
    backend: &str,
    key: &str,
    expected_size: i64,
    expected_sha256: &str,
    deep_hash: bool,
) {
    let expected_size = expected_size.max(0) as u64;
    let location = if backend == "local" {
        local_root.join(key)
    } else {
        std::path::PathBuf::from(key)
    };
    let actual = if backend == "local" {
        match tokio::fs::metadata(&location).await {
            Ok(meta) if meta.is_file() => Some(meta.len()),
            Ok(_) => {
                findings.push(missing(
                    resource,
                    backend,
                    key,
                    expected_size,
                    "对象不是普通文件",
                ));
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                findings.push(missing(resource, backend, key, expected_size, "对象不存在"));
                None
            }
            Err(error) => {
                errors.push(format!("读取本地对象 {} 失败：{}", key, error));
                None
            }
        }
    } else if let Some(s3) = s3 {
        match s3.head_object(key).await {
            Ok(size) => Some(size),
            Err(StorageError::MissingObject(detail)) => {
                findings.push(missing(resource, backend, key, expected_size, &detail));
                None
            }
            Err(error) => {
                errors.push(format!("检查 S3 对象 {} 失败：{}", key, error));
                None
            }
        }
    } else {
        errors.push(format!("对象 {} 需要 S3 后端，但当前未配置", key));
        None
    };
    let Some(actual_size) = actual else { return };
    if actual_size != expected_size {
        findings.push(ConsistencyFinding {
            kind: "size_mismatch".into(),
            resource: resource.into(),
            backend: backend.into(),
            key: key.into(),
            expected_size: Some(expected_size),
            actual_size: Some(actual_size),
            detail: Some("对象大小与 SQLite 元数据不一致".into()),
        });
    }
    if deep_hash {
        let hash = if backend == "local" {
            hash_local(&location).await
        } else {
            s3.expect("checked")
                .hash_object(key)
                .await
                .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
        };
        match hash {
            Ok((_, digest)) if !expected_sha256.is_empty() && digest != expected_sha256 => findings
                .push(ConsistencyFinding {
                    kind: "hash_mismatch".into(),
                    resource: resource.into(),
                    backend: backend.into(),
                    key: key.into(),
                    expected_size: Some(expected_size),
                    actual_size: Some(actual_size),
                    detail: Some("SHA-256 与 SQLite 元数据不一致".into()),
                }),
            Err(error) => errors.push(format!("计算对象 {} 摘要失败：{}", key, error)),
            _ => {}
        }
    }
}

async fn check_local_index(
    findings: &mut Vec<ConsistencyFinding>,
    errors: &mut Vec<String>,
    local_root: &Path,
    path: &str,
) {
    let location = local_root.join(path);
    match tokio::fs::metadata(&location).await {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => findings.push(ConsistencyFinding {
            kind: "ready_missing_object".into(),
            resource: "log_archive".into(),
            backend: "local".into(),
            key: path.into(),
            expected_size: None,
            actual_size: None,
            detail: Some("日志归档索引不是普通文件".into()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            findings.push(ConsistencyFinding {
                kind: "ready_missing_object".into(),
                resource: "log_archive".into(),
                backend: "local".into(),
                key: path.into(),
                expected_size: None,
                actual_size: None,
                detail: Some("日志归档索引不存在".into()),
            })
        }
        Err(error) => errors.push(format!("读取本地日志索引 {} 失败：{}", path, error)),
    }
}

fn missing(
    resource: &str,
    backend: &str,
    key: &str,
    expected_size: u64,
    detail: &str,
) -> ConsistencyFinding {
    ConsistencyFinding {
        kind: "ready_missing_object".into(),
        resource: resource.into(),
        backend: backend.into(),
        key: key.into(),
        expected_size: Some(expected_size),
        actual_size: None,
        detail: Some(detail.into()),
    }
}

async fn hash_local(path: &Path) -> Result<(u64, String), StoreError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Overrides};

    async fn fixture() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时目录");
        Config::load(
            dir.path().to_path_buf(),
            Overrides::default(),
            Overrides::default(),
        )
        .expect("布局");
        let pool = crate::store::bootstrap(dir.path()).await.expect("数据库");
        sqlx::query("INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at) VALUES ('demo', 'git', 'https://example.invalid/repo', 0, 0)")
            .execute(&pool).await.expect("项目");
        sqlx::query("INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at) VALUES (1, 'main', 1, 'succeeded', 'manual', '{}', 1, '{}', 0)")
            .execute(&pool).await.expect("构建");
        (dir, pool)
    }

    #[tokio::test]
    async fn read_only_check_reports_local_missing_size_and_hash() {
        let (dir, pool) = fixture().await;
        let root = dir.path().join("artifacts");
        tokio::fs::create_dir_all(root.join("1")).await.unwrap();
        tokio::fs::write(root.join("1/size.bin"), b"short")
            .await
            .unwrap();
        tokio::fs::write(root.join("1/hash.bin"), b"wrong")
            .await
            .unwrap();
        sqlx::query("INSERT INTO artifacts (build_id, backend, state, name, path, size, sha256, created_at, retention_until) VALUES (1, 'local', 'ready', 'missing.bin', '1/missing.bin', 3, 'abc', 0, 999999999999)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO artifacts (build_id, backend, state, name, path, size, sha256, created_at, retention_until) VALUES (1, 'local', 'ready', 'size.bin', '1/size.bin', 99, 'abc', 0, 999999999999)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO artifacts (build_id, backend, state, name, path, size, sha256, created_at, retention_until) VALUES (1, 'local', 'ready', 'hash.bin', '1/hash.bin', 5, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 0, 999999999999)")
            .execute(&pool).await.unwrap();

        let report = run(&pool, &root, None, true).await.unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "ready_missing_object" && f.key == "1/missing.bin")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "size_mismatch" && f.key == "1/size.bin")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "hash_mismatch" && f.key == "1/hash.bin")
        );
    }

    #[tokio::test]
    async fn backlog_is_reported_without_mutating_rows() {
        let (dir, pool) = fixture().await;
        sqlx::query("INSERT INTO artifacts (build_id, backend, state, name, path, size, sha256, created_at, retention_until) VALUES (1, 'local', 'pending', 'pending.bin', '1/pending.bin', 1, '', 0, 999999999999)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO jobs (build_id, stage_index, name, status, attempt, labels, timeout_minutes, retry_count, allow_failure) VALUES (1, 0, 'main', 'succeeded', 1, '[]', 0, 0, 0)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO log_archives (job_id, attempt, state, path, index_path, created_at) VALUES (1, 1, 'pending', 'x', 'y', 0)")
            .execute(&pool).await.unwrap();
        let report = run(&pool, &dir.path().join("artifacts"), None, false)
            .await
            .unwrap();
        assert_eq!(report.backlog.pending_uploads, 1);
        assert_eq!(report.backlog.pending_archives, 1);
        let state: String =
            sqlx::query_scalar("SELECT state FROM artifacts WHERE name='pending.bin'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "pending");
    }
}

//! S3 后端身份（票 #122，ADR-0026）：记录 endpoint/region/bucket/prefix，
//! 防止已有对象引用被误换到另一套配置。

use sqlx::SqlitePool;

use super::StoreError;

/// 已记录的 S3 后端身份（不含凭据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3BackendIdentity {
    /// Endpoint（无尾斜杠）。
    pub endpoint: String,
    /// Region。
    pub region: String,
    /// Bucket。
    pub bucket: String,
    /// 根前缀（可空）。
    pub prefix: String,
}

impl S3BackendIdentity {
    /// 由运行配置构造。
    pub fn from_config(cfg: &crate::config::S3Config) -> Self {
        Self {
            endpoint: cfg.endpoint.clone(),
            region: cfg.region.clone(),
            bucket: cfg.bucket.clone(),
            prefix: cfg.prefix.clone(),
        }
    }
}

/// 单行身份表仓储。
#[derive(Debug, Clone)]
pub struct S3IdentityRepo {
    pool: SqlitePool,
}

impl S3IdentityRepo {
    /// 从既有池装配。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 读取已记录身份（从未配置过则空）。
    pub async fn get(&self) -> Result<Option<S3BackendIdentity>, StoreError> {
        let row = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT endpoint, region, bucket, prefix FROM s3_backend_identity WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(
            row.map(|(endpoint, region, bucket, prefix)| S3BackendIdentity {
                endpoint,
                region,
                bucket,
                prefix,
            }),
        )
    }

    /// 写入或覆盖身份。
    pub async fn upsert(&self, identity: &S3BackendIdentity) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO s3_backend_identity (id, endpoint, region, bucket, prefix, recorded_at)
             VALUES (1, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               endpoint = excluded.endpoint,
               region = excluded.region,
               bucket = excluded.bucket,
               prefix = excluded.prefix,
               recorded_at = excluded.recorded_at",
        )
        .bind(&identity.endpoint)
        .bind(&identity.region)
        .bind(&identity.bucket)
        .bind(&identity.prefix)
        .bind(super::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 是否已有 S3 对象引用（产物、终态日志或仍可能迟到的归档候选）。
    pub async fn has_object_refs(&self) -> Result<bool, StoreError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM artifacts WHERE backend = 's3') +
                    (SELECT COUNT(*) FROM log_archives WHERE backend = 's3') +
                    (SELECT COUNT(*) FROM log_archive_publish_candidates)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// 启动时核对身份：已有对象引用且配置被换成另一套 → 拒绝。
    pub async fn ensure_compatible(&self, identity: &S3BackendIdentity) -> Result<(), StoreError> {
        match self.get().await? {
            None => self.upsert(identity).await,
            Some(existing) if existing == *identity => Ok(()),
            Some(existing) => {
                if self.has_object_refs().await? {
                    Err(StoreError::Invalid(format!(
                        "S3 后端配置与已有对象引用不一致：已记录 {}/{}/{} prefix={:?}，当前 {}/{}/{} prefix={:?}。请恢复原配置，或在确认无引用后更换",
                        existing.endpoint,
                        existing.region,
                        existing.bucket,
                        existing.prefix,
                        identity.endpoint,
                        identity.region,
                        identity.bucket,
                        identity.prefix
                    )))
                } else {
                    self.upsert(identity).await
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, S3IdentityRepo) {
        let dir = tempfile::tempdir().expect("临时数据目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        (dir, S3IdentityRepo::new(pool))
    }

    fn id(bucket: &str) -> S3BackendIdentity {
        S3BackendIdentity {
            endpoint: "https://s3.example.internal:9000".into(),
            region: "us-east-1".into(),
            bucket: bucket.into(),
            prefix: "prod".into(),
        }
    }

    #[tokio::test]
    async fn first_config_records_identity() {
        let (_dir, repo) = fixture().await;
        repo.ensure_compatible(&id("a")).await.expect("首次记录");
        assert_eq!(repo.get().await.unwrap().as_ref(), Some(&id("a")));
    }

    #[tokio::test]
    async fn same_identity_is_ok() {
        let (_dir, repo) = fixture().await;
        repo.ensure_compatible(&id("a")).await.unwrap();
        repo.ensure_compatible(&id("a"))
            .await
            .expect("相同身份放行");
    }

    #[tokio::test]
    async fn replacing_identity_without_objects_updates() {
        let (_dir, repo) = fixture().await;
        repo.ensure_compatible(&id("old")).await.unwrap();
        repo.ensure_compatible(&id("new"))
            .await
            .expect("无对象引用可换配置");
        assert_eq!(
            repo.get()
                .await
                .unwrap()
                .as_ref()
                .map(|i| i.bucket.as_str()),
            Some("new")
        );
    }

    #[tokio::test]
    async fn orphan_archive_candidate_still_binds_s3_identity() {
        let (_dir, repo) = fixture().await;
        repo.ensure_compatible(&id("old")).await.unwrap();
        sqlx::query("INSERT INTO log_archive_publish_candidates (key, job_id, attempt, created_at) VALUES ('prod/logs/final/old.slog', 123, 1, 0)")
            .execute(&repo.pool).await.unwrap();
        assert!(repo.has_object_refs().await.unwrap());
        assert!(matches!(
            repo.ensure_compatible(&id("new")).await,
            Err(StoreError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn replacing_identity_with_objects_is_rejected() {
        let (_dir, repo) = fixture().await;
        repo.ensure_compatible(&id("old")).await.unwrap();
        sqlx::query(
            "INSERT INTO projects (name, scm_type, scm_url, created_at, updated_at)
             VALUES ('demo', 'git', 'https://example.com/r', 0, 0)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at)
             VALUES (1, 'release', 1, 'succeeded', 'manual', '{}', 1, '{}', 0)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO artifacts
                (build_id, name, path, size, sha256, created_at, retention_until, backend, state)
             VALUES (1, 'remote.bin', 'objects/remote.bin', 3, 'abc', 0, 1, 's3', 'ready')",
        )
        .execute(&repo.pool)
        .await
        .unwrap();

        let err = repo
            .ensure_compatible(&id("new"))
            .await
            .expect_err("有对象引用时拒换配置");
        assert!(matches!(err, StoreError::Invalid(_)), "{err}");
        assert!(err.to_string().contains("old"), "{err}");
        assert_eq!(
            repo.get()
                .await
                .unwrap()
                .as_ref()
                .map(|i| i.bucket.as_str()),
            Some("old")
        );
    }
}

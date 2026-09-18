//! 产物删除生命周期的持久化模块（票 #128，ADR-0026）。

use serde::Serialize;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use super::artifacts::MultipartUploadRow;
use super::{StoreError, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
/// 异步删除目标的粒度。
pub enum DeletionScope {
    /// 单个完整产物集。
    Set,
    /// 单次构建的 S3 产物空间。
    Build,
    /// 项目拥有的全部产物与日志。
    Project,
}

impl DeletionScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Build => "build",
            Self::Project => "project",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "set" => Ok(Self::Set),
            "build" => Ok(Self::Build),
            "project" => Ok(Self::Project),
            other => Err(StoreError::Invalid(format!("未知删除范围：{other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
/// 持久化删除任务状态。
pub enum DeletionState {
    /// 等待后台执行器认领。
    Queued,
    /// 已被后台执行器认领。
    Running,
    /// 最近一次执行失败，保留目标供自动或显式重试。
    Failed,
    /// 正文与正文元数据均已清理。
    Completed,
}

impl DeletionState {
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "failed" => Ok(Self::Failed),
            "completed" => Ok(Self::Completed),
            other => Err(StoreError::Invalid(format!("未知删除状态：{other}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
/// 可供 REST 与后台共用的删除任务视图。
pub struct DeletionJob {
    /// 删除任务 ID。
    pub id: i64,
    /// 归属项目 ID；项目墓碑保留该 ID。
    pub project_id: i64,
    /// 项目名快照；项目冻结后仍用于运维辨识。
    pub project_name: String,
    /// 删除粒度。
    pub scope: DeletionScope,
    /// 当前状态。
    pub state: DeletionState,
    /// 构建范围的流水线名。
    pub pipeline_name: Option<String>,
    /// 构建范围的构建号。
    pub build_number: Option<i64>,
    /// 产物集范围的稳定 ID。
    pub set_id: Option<i64>,
    /// 后台认领次数。
    pub attempts: i64,
    /// 最近一次失败信息。
    pub last_error: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最近更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
/// 异步删除队列的持久化仓储。
pub struct DeletionRepo {
    pool: SqlitePool,
}

/// 后台执行器认领到的内部目标。
#[derive(Debug, Clone)]
pub struct ClaimedDeletion {
    /// 删除任务 id。
    pub id: i64,
    /// 删除范围。
    pub scope: DeletionScope,
    /// 项目 id。
    pub project_id: i64,
    /// 构建 id（set/build 范围存在）。
    pub build_id: Option<i64>,
    /// 产物集 id（set 范围存在）。
    pub set_id: Option<i64>,
}

/// 尚未发布的 S3 临时对象定位信息。完整对象键在执行器中结合当前后端
/// prefix 生成，避免把部署配置写入持久化仓储。
#[derive(Debug, Clone)]
pub struct PendingArtifactObject {
    /// 构建行 id。
    pub build_id: i64,
    /// 产生对象的任务行 id。
    pub job_id: i64,
    /// 任务重试序号。
    pub attempt: i32,
    /// 产物逻辑名。
    pub name: String,
}

type DeletionRow = (
    i64,
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    i64,
    Option<String>,
    i64,
    i64,
);

impl DeletionRepo {
    /// 由已迁移的连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 构建级删除幂等入队；失败任务再次请求会回到 queued。
    pub async fn enqueue_build(
        &self,
        project_id: i64,
        build_id: i64,
        requested_by: &str,
    ) -> Result<DeletionJob, StoreError> {
        let snapshot: String = sqlx::query_scalar(
            "SELECT json_object(
                'project', p.name, 'pipeline', b.pipeline_name, 'build_number', b.number,
                'trigger', b.trigger, 'trigger_detail', b.trigger_detail, 'snapshot', b.snapshot)
             FROM builds b JOIN projects p ON p.id = b.project_id
             WHERE b.id = ? AND b.project_id = ?",
        )
        .bind(build_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("构建 {build_id}")))?;
        let now = now_ms();
        sqlx::query(
            "INSERT INTO artifact_deletions
                (project_id, build_id, set_id, scope, state, attempts, last_error,
                 next_attempt_at, requested_by, source_snapshot, created_at, updated_at)
             VALUES (?, ?, NULL, 'build', 'queued', 0, NULL, ?, ?, ?, ?, ?)
             ON CONFLICT DO UPDATE SET
                 state = CASE WHEN artifact_deletions.state = 'failed' THEN 'queued'
                              ELSE artifact_deletions.state END,
                 last_error = CASE WHEN artifact_deletions.state = 'failed' THEN NULL
                                   ELSE artifact_deletions.last_error END,
                 next_attempt_at = CASE WHEN artifact_deletions.state = 'failed' THEN excluded.next_attempt_at
                                        ELSE artifact_deletions.next_attempt_at END,
                 updated_at = excluded.updated_at",
        )
        .bind(project_id)
        .bind(build_id)
        .bind(now)
        .bind(requested_by)
        .bind(snapshot)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.find_active(DeletionScope::Build, project_id, Some(build_id), None)
            .await?
            .ok_or_else(|| StoreError::NotFound("刚创建的删除任务".into()))
    }

    /// 产物集级删除幂等入队；调用方已确认构建终态和项目权限。
    pub async fn enqueue_set(
        &self,
        project_id: i64,
        build_id: i64,
        set_id: i64,
        requested_by: &str,
    ) -> Result<DeletionJob, StoreError> {
        let snapshot: String = sqlx::query_scalar(
            "SELECT json_object(
                'project', p.name, 'pipeline', b.pipeline_name, 'build_number', b.number,
                'set_id', s.id, 'set_name', s.name, 'job_id', s.job_id, 'attempt', s.attempt)
             FROM artifact_sets s
             JOIN builds b ON b.id = s.build_id
             JOIN projects p ON p.id = b.project_id
             WHERE s.id = ? AND s.build_id = ? AND b.project_id = ? AND s.state = 'ready'",
        )
        .bind(set_id)
        .bind(build_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("产物集 {set_id}")))?;
        let now = now_ms();
        sqlx::query(
            "INSERT INTO artifact_deletions
                (project_id, build_id, set_id, scope, state, attempts, last_error,
                 next_attempt_at, requested_by, source_snapshot, created_at, updated_at)
             VALUES (?, ?, ?, 'set', 'queued', 0, NULL, ?, ?, ?, ?, ?)
             ON CONFLICT DO UPDATE SET
                 state = CASE WHEN artifact_deletions.state = 'failed' THEN 'queued'
                              ELSE artifact_deletions.state END,
                 last_error = CASE WHEN artifact_deletions.state = 'failed' THEN NULL
                                   ELSE artifact_deletions.last_error END,
                 next_attempt_at = CASE WHEN artifact_deletions.state = 'failed' THEN excluded.next_attempt_at
                                        ELSE artifact_deletions.next_attempt_at END,
                 updated_at = excluded.updated_at",
        )
        .bind(project_id)
        .bind(build_id)
        .bind(set_id)
        .bind(now)
        .bind(requested_by)
        .bind(snapshot)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.find_active(DeletionScope::Set, project_id, Some(build_id), Some(set_id))
            .await?
            .ok_or_else(|| StoreError::NotFound("刚创建的删除任务".into()))
    }

    /// 项目删除入队并在同一事务内冻结授权。只允许所有构建均已终态时受理。
    pub async fn enqueue_project(
        &self,
        project_id: i64,
        requested_by: &str,
    ) -> Result<DeletionJob, StoreError> {
        let mut tx = self.pool.begin().await?;
        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM builds
             WHERE project_id = ? AND status NOT IN ('succeeded', 'failed', 'cancelled', 'timeout')",
        )
        .bind(project_id)
        .fetch_one(&mut *tx)
        .await?;
        if live != 0 {
            return Err(StoreError::Conflict("项目仍有排队或运行中的构建".into()));
        }
        let snapshot: String = sqlx::query_scalar(
            "SELECT json_object(
                'project_id', id, 'name', name, 'scm_type', scm_type,
                'scm_url', scm_url, 'default_branch', default_branch,
                'created_at', created_at, 'updated_at', updated_at)
             FROM projects WHERE id = ? AND lifecycle = 'active'",
        )
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("项目 {project_id}")))?;
        let now = now_ms();
        sqlx::query(
            "INSERT INTO artifact_deletions
                (project_id, build_id, set_id, scope, state, attempts, last_error,
                 next_attempt_at, requested_by, source_snapshot, created_at, updated_at)
             VALUES (?, NULL, NULL, 'project', 'queued', 0, NULL, ?, ?, ?, ?, ?)",
        )
        .bind(project_id)
        .bind(now)
        .bind(requested_by)
        .bind(snapshot)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE projects SET lifecycle = 'deleting', updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.find_active(DeletionScope::Project, project_id, None, None)
            .await?
            .ok_or_else(|| StoreError::NotFound("刚创建的项目删除任务".into()))
    }

    /// 列出项目管理员可观察的全部删除任务。
    pub async fn list_by_project(&self, project_id: i64) -> Result<Vec<DeletionJob>, StoreError> {
        let rows = sqlx::query_as::<_, DeletionRow>(
            "SELECT d.id, d.project_id, p.name, d.scope, d.state, b.pipeline_name, b.number,
                    d.set_id, d.attempts, d.last_error, d.created_at, d.updated_at
             FROM artifact_deletions d
             JOIN projects p ON p.id = d.project_id
             LEFT JOIN builds b ON b.id = d.build_id
             WHERE d.project_id = ? ORDER BY d.created_at DESC, d.id DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Self::from_row).collect()
    }

    /// 全局管理员查看项目删除任务，包括已冻结和已完成的项目。
    pub async fn list_project_deletions(&self) -> Result<Vec<DeletionJob>, StoreError> {
        let rows = sqlx::query_as::<_, DeletionRow>(
            "SELECT d.id, d.project_id, p.name, d.scope, d.state, NULL, NULL,
                    NULL, d.attempts, d.last_error, d.created_at, d.updated_at
             FROM artifact_deletions d JOIN projects p ON p.id = d.project_id
             WHERE d.scope = 'project' ORDER BY d.created_at DESC, d.id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Self::from_row).collect()
    }

    /// 将本项目的失败任务显式重新排队；其它状态保持幂等原状。
    pub async fn retry(&self, project_id: i64, id: i64) -> Result<Option<DeletionJob>, StoreError> {
        let now = now_ms();
        sqlx::query(
            "UPDATE artifact_deletions
             SET state = 'queued', last_error = NULL, next_attempt_at = ?, updated_at = ?
             WHERE id = ? AND project_id = ? AND state = 'failed'",
        )
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(project_id)
        .execute(&self.pool)
        .await?;
        self.get(id, project_id).await
    }

    /// 全局管理员将失败的项目清理重新排队；其它状态幂等返回原任务。
    pub async fn retry_project(&self, id: i64) -> Result<Option<DeletionJob>, StoreError> {
        let now = now_ms();
        sqlx::query(
            "UPDATE artifact_deletions
             SET state = 'queued', last_error = NULL, next_attempt_at = ?, updated_at = ?
             WHERE id = ? AND scope = 'project' AND state = 'failed'",
        )
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_by_id(id, DeletionScope::Project).await
    }

    /// 原子认领一个到期任务并增加尝试次数。running 任务在进程重启后也可
    /// 被重新认领，依赖对象 DELETE 的幂等语义恢复。
    pub async fn claim_next(&self, now: i64) -> Result<Option<ClaimedDeletion>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let candidate = sqlx::query_as::<_, (i64, String, i64, Option<i64>, Option<i64>)>(
            "SELECT id, scope, project_id, build_id, set_id
             FROM artifact_deletions
             WHERE (state IN ('queued', 'failed') AND next_attempt_at <= ?) OR state = 'running'
             ORDER BY CASE state WHEN 'running' THEN 0 ELSE 1 END, created_at, id
             LIMIT 1",
        )
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, scope, project_id, build_id, set_id)) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        sqlx::query(
            "UPDATE artifact_deletions
             SET state = 'running', attempts = attempts + 1, last_error = NULL, updated_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(ClaimedDeletion {
            id,
            scope: DeletionScope::parse(&scope)?,
            project_id,
            build_id,
            set_id,
        }))
    }

    /// 列出任务范围内的 S3 正文键。
    pub async fn object_keys(&self, job: &ClaimedDeletion) -> Result<Vec<String>, StoreError> {
        let keys =
            match job.scope {
                DeletionScope::Build => sqlx::query_scalar(
                    "SELECT path FROM artifacts WHERE build_id = ? AND backend = 's3' ORDER BY id",
                )
                .bind(job.build_id)
                .fetch_all(&self.pool)
                .await?,
                DeletionScope::Set => {
                    sqlx::query_scalar(
                        "SELECT a.path FROM artifacts a
                     JOIN artifact_set_entries e ON e.artifact_name = a.name
                     WHERE e.set_id = ? AND a.build_id = ? AND a.backend = 's3'
                     ORDER BY a.id",
                    )
                    .bind(job.set_id)
                    .bind(job.build_id)
                    .fetch_all(&self.pool)
                    .await?
                }
                DeletionScope::Project => {
                    sqlx::query_scalar(
                        "SELECT a.path FROM artifacts a JOIN builds b ON b.id = a.build_id
                     WHERE b.project_id = ? AND a.backend = 's3' ORDER BY a.id",
                    )
                    .bind(job.project_id)
                    .fetch_all(&self.pool)
                    .await?
                }
            };
        Ok(keys)
    }

    /// 列出构建/项目范围内尚未发布完成的 multipart 临时会话。产物集只在
    /// ready 后可删除，不会拥有仍在传输的会话。
    pub async fn multipart_uploads(
        &self,
        job: &ClaimedDeletion,
    ) -> Result<Vec<MultipartUploadRow>, StoreError> {
        type MultipartRow = (i64, String, i64, i32, String, String, i64, i64, bool, i64);
        let rows: Vec<MultipartRow> = match job.scope {
            DeletionScope::Build => {
                sqlx::query_as(
                    "SELECT build_id, name, job_id, attempt, object_key, upload_id,
                        size, part_size, completed, expires_at
                 FROM artifact_multipart_uploads WHERE build_id = ? ORDER BY name",
                )
                .bind(job.build_id)
                .fetch_all(&self.pool)
                .await?
            }
            DeletionScope::Project => {
                sqlx::query_as(
                    "SELECT m.build_id, m.name, m.job_id, m.attempt, m.object_key,
                        m.upload_id, m.size, m.part_size, m.completed, m.expires_at
                 FROM artifact_multipart_uploads m
                 JOIN builds b ON b.id = m.build_id
                 WHERE b.project_id = ? ORDER BY m.build_id, m.name",
                )
                .bind(job.project_id)
                .fetch_all(&self.pool)
                .await?
            }
            DeletionScope::Set => Vec::new(),
        };
        Ok(rows
            .into_iter()
            .map(|row| MultipartUploadRow {
                build_id: row.0,
                name: row.1,
                job_id: row.2,
                attempt: row.3,
                object_key: row.4,
                upload_id: row.5,
                size: row.6.max(0) as u64,
                part_size: row.7.max(0) as u64,
                completed: row.8,
                expires_at: row.9,
            })
            .collect())
    }

    /// 列出构建/项目范围内尚未 complete 的 S3 对象。其 `artifacts.path`
    /// 指向最终键，因此需另外重建曾签发写权限的临时键。
    pub async fn pending_artifact_objects(
        &self,
        job: &ClaimedDeletion,
    ) -> Result<Vec<PendingArtifactObject>, StoreError> {
        let rows: Vec<(i64, i64, i32, String)> = match job.scope {
            DeletionScope::Build => {
                sqlx::query_as(
                    "SELECT build_id, job_id, attempt, name FROM artifacts
                 WHERE build_id = ? AND backend = 's3' AND state = 'pending'
                   AND job_id IS NOT NULL AND attempt IS NOT NULL
                 ORDER BY id",
                )
                .bind(job.build_id)
                .fetch_all(&self.pool)
                .await?
            }
            DeletionScope::Project => {
                sqlx::query_as(
                    "SELECT a.build_id, a.job_id, a.attempt, a.name
                 FROM artifacts a JOIN builds b ON b.id = a.build_id
                 WHERE b.project_id = ? AND a.backend = 's3' AND a.state = 'pending'
                   AND a.job_id IS NOT NULL AND a.attempt IS NOT NULL
                 ORDER BY a.id",
                )
                .bind(job.project_id)
                .fetch_all(&self.pool)
                .await?
            }
            DeletionScope::Set => Vec::new(),
        };
        Ok(rows
            .into_iter()
            .map(|(build_id, job_id, attempt, name)| PendingArtifactObject {
                build_id,
                job_id,
                attempt,
                name,
            })
            .collect())
    }

    /// 项目清理的构建清单；本地正文与旧日志按构建复用既有清理器。
    pub async fn project_build_ids(&self, project_id: i64) -> Result<Vec<i64>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM builds WHERE project_id = ? ORDER BY id")
                .bind(project_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 全部对象删除成功后，在一笔事务里裁剪正文元数据并完成任务。
    pub async fn complete(&self, job: &ClaimedDeletion) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        match job.scope {
            DeletionScope::Build => {
                sqlx::query("DELETE FROM artifact_multipart_uploads WHERE build_id = ?")
                    .bind(job.build_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM artifact_sets WHERE build_id = ?")
                    .bind(job.build_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM artifacts WHERE build_id = ? AND backend = 's3'")
                    .bind(job.build_id)
                    .execute(&mut *tx)
                    .await?;
            }
            DeletionScope::Set => {
                sqlx::query(
                    "DELETE FROM artifacts WHERE build_id = ? AND name IN
                        (SELECT artifact_name FROM artifact_set_entries
                         WHERE set_id = ? AND artifact_name IS NOT NULL)",
                )
                .bind(job.build_id)
                .bind(job.set_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM artifact_sets WHERE id = ?")
                    .bind(job.set_id)
                    .execute(&mut *tx)
                    .await?;
            }
            DeletionScope::Project => {
                sqlx::query(
                    "DELETE FROM artifact_multipart_uploads WHERE build_id IN
                        (SELECT id FROM builds WHERE project_id = ?)",
                )
                .bind(job.project_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "DELETE FROM artifact_sets WHERE build_id IN
                        (SELECT id FROM builds WHERE project_id = ?)",
                )
                .bind(job.project_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "DELETE FROM artifacts WHERE build_id IN
                        (SELECT id FROM builds WHERE project_id = ?)",
                )
                .bind(job.project_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "DELETE FROM logs WHERE build_id IN
                        (SELECT id FROM builds WHERE project_id = ?)",
                )
                .bind(job.project_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE projects SET lifecycle = 'deleted', updated_at = ? WHERE id = ?",
                )
                .bind(now_ms())
                .bind(job.project_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "UPDATE artifact_deletions
             SET state = 'completed', last_error = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(now_ms())
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 失败保留目标与元数据，记录错误并安排自动重试。
    pub async fn fail(&self, id: i64, error: &str) -> Result<(), StoreError> {
        let now = now_ms();
        sqlx::query(
            "UPDATE artifact_deletions
             SET state = 'failed', last_error = ?, next_attempt_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(error)
        .bind(now + 60_000)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: i64, project_id: i64) -> Result<Option<DeletionJob>, StoreError> {
        let row = sqlx::query_as::<_, DeletionRow>(
            "SELECT d.id, d.project_id, p.name, d.scope, d.state, b.pipeline_name, b.number,
                    d.set_id, d.attempts, d.last_error, d.created_at, d.updated_at
             FROM artifact_deletions d
             JOIN projects p ON p.id = d.project_id
             LEFT JOIN builds b ON b.id = d.build_id
             WHERE d.id = ? AND d.project_id = ?",
        )
        .bind(id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Self::from_row).transpose()
    }

    async fn get_by_id(
        &self,
        id: i64,
        scope: DeletionScope,
    ) -> Result<Option<DeletionJob>, StoreError> {
        let row = sqlx::query_as::<_, DeletionRow>(
            "SELECT d.id, d.project_id, p.name, d.scope, d.state, b.pipeline_name, b.number,
                    d.set_id, d.attempts, d.last_error, d.created_at, d.updated_at
             FROM artifact_deletions d
             JOIN projects p ON p.id = d.project_id
             LEFT JOIN builds b ON b.id = d.build_id
             WHERE d.id = ? AND d.scope = ?",
        )
        .bind(id)
        .bind(scope.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Self::from_row).transpose()
    }

    async fn find_active(
        &self,
        scope: DeletionScope,
        project_id: i64,
        build_id: Option<i64>,
        set_id: Option<i64>,
    ) -> Result<Option<DeletionJob>, StoreError> {
        let row = sqlx::query_as::<_, DeletionRow>(
            "SELECT d.id, d.project_id, p.name, d.scope, d.state, b.pipeline_name, b.number,
                    d.set_id, d.attempts, d.last_error, d.created_at, d.updated_at
             FROM artifact_deletions d
             JOIN projects p ON p.id = d.project_id
             LEFT JOIN builds b ON b.id = d.build_id
             WHERE d.scope = ? AND d.project_id = ?
               AND d.build_id IS ? AND d.set_id IS ? AND d.state != 'completed'",
        )
        .bind(scope.as_str())
        .bind(project_id)
        .bind(build_id)
        .bind(set_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Self::from_row).transpose()
    }

    fn from_row(row: DeletionRow) -> Result<DeletionJob, StoreError> {
        Ok(DeletionJob {
            id: row.0,
            project_id: row.1,
            project_name: row.2,
            scope: DeletionScope::parse(&row.3)?,
            state: DeletionState::parse(&row.4)?,
            pipeline_name: row.5,
            build_number: row.6,
            set_id: row.7,
            attempts: row.8,
            last_error: row.9,
            created_at: row.10,
            updated_at: row.11,
        })
    }
}

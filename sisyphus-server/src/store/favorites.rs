//! 用户级流水线收藏仓储（票 #137）。

use sqlx::SqlitePool;

use super::{StoreError, now_ms};
use crate::store::builds::BuildStatus;

/// 收藏清单行；最近构建字段由同一查询关联，避免工作台逐条请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FavoriteRow {
    /// 项目名。
    pub project: String,
    /// 流水线名。
    pub pipeline: String,
    /// 收藏时间（Unix 毫秒）。
    pub added_at: i64,
    /// 最近构建号；从未构建为空。
    pub latest_number: Option<i64>,
    /// 最近构建状态；从未构建为空。
    pub latest_status: Option<BuildStatus>,
    /// 最近构建开始时间。
    pub latest_started_at: Option<i64>,
    /// 最近构建结束时间。
    pub latest_finished_at: Option<i64>,
}

/// 收藏仓储。
#[derive(Debug, Clone)]
pub struct FavoriteRepo {
    pool: SqlitePool,
}

impl FavoriteRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 按收藏时间倒序列出当前用户收藏，并关联每条流水线的最新真实构建。
    pub async fn list_by_user(
        &self,
        user_id: i64,
        is_admin: bool,
    ) -> Result<Vec<FavoriteRow>, StoreError> {
        let rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                i64,
                Option<i64>,
                Option<String>,
                Option<i64>,
                Option<i64>,
            ),
        >(
            "SELECT project.name, favorite.pipeline_name, favorite.added_at,
                    build.number, build.status, build.started_at, build.finished_at
             FROM pipeline_favorites favorite
             JOIN projects project ON project.id = favorite.project_id
             LEFT JOIN builds build ON build.id = (
                 SELECT candidate.id FROM builds candidate
                 WHERE candidate.project_id = favorite.project_id
                   AND candidate.pipeline_name = favorite.pipeline_name
                 ORDER BY candidate.number DESC
                 LIMIT 1
             )
             WHERE favorite.user_id = ?
               AND (? OR EXISTS(
                   SELECT 1 FROM project_members member
                   WHERE member.project_id = favorite.project_id
                     AND member.user_id = ?
               ))
             ORDER BY favorite.added_at DESC, project.name, favorite.pipeline_name",
        )
        .bind(user_id)
        .bind(is_admin)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(
                |(
                    project,
                    pipeline,
                    added_at,
                    latest_number,
                    latest_status,
                    latest_started_at,
                    latest_finished_at,
                )| {
                    Ok(FavoriteRow {
                        project,
                        pipeline,
                        added_at,
                        latest_number,
                        latest_status: latest_status
                            .map(|status| BuildStatus::parse(&status))
                            .transpose()?,
                        latest_started_at,
                        latest_finished_at,
                    })
                },
            )
            .collect()
    }

    /// 收藏已存在的流水线。
    pub async fn add(
        &self,
        user_id: i64,
        project_id: i64,
        pipeline: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT OR IGNORE INTO pipeline_favorites
                (user_id, project_id, pipeline_name, added_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(pipeline)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 取消收藏；目标未收藏时仍成功。
    pub async fn remove(
        &self,
        user_id: i64,
        project_id: i64,
        pipeline: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM pipeline_favorites
             WHERE user_id = ?
               AND project_id = ?
               AND pipeline_name = ?",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(pipeline)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

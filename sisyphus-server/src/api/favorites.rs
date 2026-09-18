//! 当前用户的流水线收藏端点（票 #137）。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::builds::BuildStatusDto;
use super::error::ApiError;
use super::policy::RequireViewer;
use crate::store::favorites::FavoriteRow;

/// 收藏条目内嵌的最近构建摘要。
#[derive(Debug, Serialize, ToSchema)]
pub struct FavoriteLatestBuildResponse {
    /// per-pipeline 构建号。
    pub number: i64,
    /// 构建状态。
    pub status: BuildStatusDto,
    /// 开始时刻（Unix 毫秒）。
    pub started_at: Option<i64>,
    /// 终态时刻（Unix 毫秒）。
    pub finished_at: Option<i64>,
}

/// 当前用户的一条流水线收藏。
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineFavoriteResponse {
    /// 项目名。
    pub project: String,
    /// 流水线名。
    pub pipeline: String,
    /// 收藏时刻（Unix 毫秒）。
    pub added_at: i64,
    /// 最近构建；从未运行为空。
    pub latest_build: Option<FavoriteLatestBuildResponse>,
}

impl From<FavoriteRow> for PipelineFavoriteResponse {
    fn from(row: FavoriteRow) -> Self {
        let latest_build = row
            .latest_number
            .zip(row.latest_status)
            .map(|(number, status)| FavoriteLatestBuildResponse {
                number,
                status: status.into(),
                started_at: row.latest_started_at,
                finished_at: row.latest_finished_at,
            });
        Self {
            project: row.project,
            pipeline: row.pipeline,
            added_at: row.added_at,
            latest_build,
        }
    }
}

/// 列出当前用户收藏的流水线。
#[utoipa::path(
    get,
    path = "/api/v1/user/pipeline-favorites",
    tag = "favorites",
    responses(
        (status = 200, description = "当前用户可见的收藏清单（收藏时间倒序，内嵌最新真实构建）", body = [PipelineFavoriteResponse]),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<PipelineFavoriteResponse>>, ApiError> {
    let rows = state
        .favorites
        .list_by_user(auth.user_id, auth.is_admin)
        .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// 收藏已存在的流水线。
#[utoipa::path(
    put,
    path = "/api/v1/user/pipeline-favorites/{name}/{pipeline}",
    tag = "favorites",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "流水线名"),
    ),
    responses(
        (status = 204, description = "收藏成功；重复收藏同样成功"),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
        (status = 404, description = "流水线不存在或调用者不可见", body = super::error::ErrorBody),
    )
)]
pub async fn add(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path((_name, pipeline)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_pipeline(&state, &access.project.name, &pipeline).await?;
    state
        .favorites
        .add(auth.user_id, access.project.id, &pipeline)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 取消收藏；未收藏时同样成功。
#[utoipa::path(
    delete,
    path = "/api/v1/user/pipeline-favorites/{name}/{pipeline}",
    tag = "favorites",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "流水线名"),
    ),
    responses(
        (status = 204, description = "取消成功；未收藏时同样成功"),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
        (status = 404, description = "流水线不存在或调用者不可见", body = super::error::ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path((_name, pipeline)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_pipeline(&state, &access.project.name, &pipeline).await?;
    state
        .favorites
        .remove(auth.user_id, access.project.id, &pipeline)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn require_pipeline(state: &AppState, project: &str, pipeline: &str) -> Result<(), ApiError> {
    state
        .pipelines
        .get(project, pipeline)
        .await?
        .map(|_| ())
        .ok_or_else(|| ApiError::resource_not_found("流水线不存在"))
}

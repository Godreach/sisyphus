//! 项目管理员可观察的异步删除任务（票 #128）。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;
use utoipa::ToSchema;

use super::{
    AppState,
    error::ApiError,
    policy::{RequireAdmin, RequireGlobalAdmin},
};
use crate::store::deletions::DeletionJob;

#[derive(Debug, Serialize, ToSchema)]
/// 删除任务列表响应。
pub struct DeletionJobsResponse {
    /// 按创建时间倒序的删除任务。
    pub items: Vec<DeletionJob>,
}

/// 项目管理员查看本项目的产物删除队列。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/artifact-deletions",
    tag = "artifacts",
    responses(
        (status = 200, description = "项目删除任务与失败信息", body = DeletionJobsResponse),
        (status = 403, description = "需项目 admin 档", body = super::error::ErrorBody),
        (status = 404, description = "项目不存在或不可见", body = super::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
) -> Result<Json<DeletionJobsResponse>, ApiError> {
    Ok(Json(DeletionJobsResponse {
        items: state.deletions.list_by_project(access.project.id).await?,
    }))
}

/// 将失败删除任务显式重新排队；重复请求保持当前状态。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/artifact-deletions/{id}/retry",
    tag = "artifacts",
    responses(
        (status = 202, description = "已重新排队", body = DeletionJob),
        (status = 403, description = "需项目 admin 档", body = super::error::ErrorBody),
        (status = 404, description = "任务不存在", body = super::error::ErrorBody),
    )
)]
pub async fn retry(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, id)): Path<(String, i64)>,
) -> Result<(StatusCode, Json<DeletionJob>), ApiError> {
    let job = state
        .deletions
        .retry(access.project.id, id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found("删除任务不存在"))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// 全局管理员查看项目删除队列；冻结项目不再能经项目授权面访问。
#[utoipa::path(
    get,
    path = "/api/v1/project-deletions",
    tag = "projects",
    responses(
        (status = 200, description = "项目删除任务", body = DeletionJobsResponse),
        (status = 403, description = "仅全局管理员可见", body = super::error::ErrorBody),
    )
)]
pub async fn list_projects(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<DeletionJobsResponse>, ApiError> {
    Ok(Json(DeletionJobsResponse {
        items: state.deletions.list_project_deletions().await?,
    }))
}

/// 全局管理员将失败的项目清理重新排队。
#[utoipa::path(
    post,
    path = "/api/v1/project-deletions/{id}/retry",
    tag = "projects",
    responses(
        (status = 202, description = "项目清理已重新排队", body = DeletionJob),
        (status = 403, description = "仅全局管理员可操作", body = super::error::ErrorBody),
        (status = 404, description = "项目删除任务不存在", body = super::error::ErrorBody),
    )
)]
pub async fn retry_project(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<DeletionJob>), ApiError> {
    let job = state
        .deletions
        .retry_project(id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found("项目删除任务不存在"))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

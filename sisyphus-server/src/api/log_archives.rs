//! 独立日志状态与管理员待归档/丢失管理面（#132）。

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

use super::{
    AppState,
    error::{ApiError, ErrorBody},
    policy::{RequireGlobalAdmin, RequireViewer},
};
use crate::store::ArchiveStatus;

/// 管理列表可限定 Agent。
#[derive(Deserialize, IntoParams)]
pub struct BacklogQuery {
    /// 构建机名。
    pub agent: Option<String>,
    /// 每页最多 500 条，从指定偏移读取；待归档优先。
    pub offset: Option<u32>,
}

/// 不可恢复或强制清理必须填写原因。
#[derive(Deserialize, ToSchema)]
pub struct LostRequest {
    /// 可审计的原因。
    pub reason: String,
}

#[utoipa::path(get, path="/api/v1/log-archives", tag="agents", params(BacklogQuery),
    responses((status=200, body=Vec<ArchiveStatus>), (status=401, body=ErrorBody), (status=403, body=ErrorBody)))]
/// 管理员查看待归档及丢失记录。
pub async fn backlog(
    State(state): State<AppState>,
    _: RequireGlobalAdmin,
    Query(query): Query<BacklogQuery>,
) -> Result<Json<Vec<ArchiveStatus>>, ApiError> {
    Ok(Json(
        state
            .log_archives
            .backlog(query.agent.as_deref(), query.offset.unwrap_or(0))
            .await?,
    ))
}

#[utoipa::path(post, path="/api/v1/log-archives/{job_id}/{attempt}/lost", tag="agents",
    params(("job_id"=i64, Path), ("attempt"=i32, Path)), request_body=LostRequest,
    responses((status=200, body=ArchiveStatus), (status=401, body=ErrorBody), (status=403, body=ErrorBody),
        (status=409, body=ErrorBody), (status=422, body=ErrorBody)))]
/// 管理员确认永久丢失，原因写入审计。
pub async fn mark_lost(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path((job, attempt)): Path<(i64, i32)>,
    Json(request): Json<LostRequest>,
) -> Result<Json<ArchiveStatus>, ApiError> {
    let reason = request.reason.trim();
    if reason.is_empty() || reason.len() > 2000 {
        return Err(ApiError::validation(
            "必须填写丢失原因（最多 2000 字节）",
            vec![],
        ));
    }
    state
        .log_archives
        .mark_lost(job, attempt, reason, &auth.username)
        .await?;
    if let Err(error) =
        crate::store::cleanup::cleanup_lost_archives(&state.pool, state.s3.as_deref()).await
    {
        tracing::warn!(job, attempt, error = %error, "丢失日志正文清理失败，日常扫描重试");
    }
    Ok(Json(
        state
            .log_archives
            .status(job, attempt)
            .await?
            .ok_or_else(|| ApiError::resource_not_found("日志归档不存在"))?,
    ))
}

#[utoipa::path(get, path="/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/jobs/{job}/attempts/{attempt}/logs/status", tag="builds",
    params(("name"=String, Path), ("pipeline"=String, Path), ("number"=i64, Path), ("job"=String, Path), ("attempt"=i32, Path)),
    responses((status=200, body=Option<ArchiveStatus>), (status=401, body=ErrorBody), (status=403, body=ErrorBody), (status=404, body=ErrorBody)))]
/// 项目 viewer 查看独立归档状态。
pub async fn status(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number, job_name, attempt)): Path<(String, String, i64, String, i32)>,
) -> Result<Json<Option<ArchiveStatus>>, ApiError> {
    let build = super::builds::load_build(&state, &access.project.id, &pipeline, number).await?;
    let job = super::logs::load_job(&state, &build, &job_name, attempt).await?;
    Ok(Json(state.log_archives.status(job.id, attempt).await?))
}

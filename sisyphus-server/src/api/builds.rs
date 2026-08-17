//! 构建 REST 端点（票 B2c-T5，ADR-0006/0008）：触发 / 取消 / 重跑 / 列表 /
//! 详情。消费 #46 engine 编排状态机与 #48 调度下发。
//!
//! - **授权**：触发 / 取消 / 重跑 runner 档（[`Permission::Run`]，B2b 留的
//!   `#[allow(dead_code)]` 兑现）；列表 / 详情 viewer 档。无角色项目 404 同
//!   纪律（授权 extractor 先裁决，不泄存在性）。
//! - **触发**：runner 档，参数覆盖默认值语义 + 可选 git 分支/commit 或 svn
//!   revision；调 [`Engine::start_build`]（engine 校验 project/pipeline 存在
//!   → 404）。返回 202 + 构建号。
//! - **取消**：runner 档，调 [`Engine::cancel_build`]（DB 迁移 + 发
//!   `BuildStatus{Cancelled}` 事件，sched 据此经通道下发 CancelBuild 到在途
//!   任务——与 fail-fast 同款事件路径）。终态幂等 202。
//! - **重跑**：`from_scratch` 经 [`Engine::start_build`]（新号 attempt=1，复制
//!   原触发上下文、by 改当前用户）；`from_failed` 经
//!   [`BuildRepo::rerun_from_failed`]（同号 attempt+1，成功任务保留）+
//!   [`Engine::drive`]（重开失败任务）。非 failed/cancelled/timeout 终态 → 409。
//! - **列表**：倒序 + 分页(page/limit) + 状态过滤；**详情**：状态/触发人/
//!   attempt/耗时/阶段与任务状态（阶段名取自构建快照）。
//!
//! 缺机密名任务在 engine 组装期即 failed（detail 记名、不泄值，ADR-0015），
//! 走 fail-fast 级联——本模块只读回该结果，不在 REST 层复刻机密逻辑。

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sisyphus_model::validate::BuildSnapshot;
use utoipa::{IntoParams, ToSchema};
use utoipa::openapi::schema::{ObjectBuilder, Type};

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::{RequireRunner, RequireViewer};
use crate::engine::{ParameterOverride, StartBuildInput, TriggerDetail};
use crate::store::builds::{BuildRepo, BuildRow, BuildStatus, TriggerSource};
use crate::store::jobs::{JobRepo, JobStatus};
use crate::store::now_ms;

/// 列表单页条数上限（防拖全表；页大小由调用侧在 limit 内自选）。
pub const BUILDS_PAGE_MAX: i64 = 100;
/// 列表单页条数缺省。
const BUILDS_PAGE_DEFAULT: i64 = 20;
/// 页码缺省（从 1 起）。
const BUILDS_PAGE_NUMBER_DEFAULT: i64 = 1;

// ---------------------------------------------------------------------------
// DTO（store 枚举未派生 serde；API 层自有形态，落库文本同型）
// ---------------------------------------------------------------------------

/// 构建状态（REST 形态；落库文本同型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BuildStatusDto {
    /// 已排队。
    Queued,
    /// 运行中。
    Running,
    /// 全部任务成功。
    Succeeded,
    /// 任务失败（含 fail-fast 级联）。
    Failed,
    /// 构建级取消。
    Cancelled,
    /// 任务超时走取消路径的终态。
    Timeout,
}

impl From<BuildStatus> for BuildStatusDto {
    fn from(s: BuildStatus) -> Self {
        match s {
            BuildStatus::Queued => Self::Queued,
            BuildStatus::Running => Self::Running,
            BuildStatus::Succeeded => Self::Succeeded,
            BuildStatus::Failed => Self::Failed,
            BuildStatus::Cancelled => Self::Cancelled,
            BuildStatus::Timeout => Self::Timeout,
        }
    }
}

/// 构建触发源（手动/cron/poll）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TriggerSourceDto {
    /// 手动触发。
    Manual,
    /// cron 定时触发。
    Cron,
    /// poll SCM 轮询触发。
    Poll,
}

impl From<TriggerSource> for TriggerSourceDto {
    fn from(s: TriggerSource) -> Self {
        match s {
            TriggerSource::Manual => Self::Manual,
            TriggerSource::Cron => Self::Cron,
            TriggerSource::Poll => Self::Poll,
        }
    }
}

/// 任务状态（详情视图；落库文本同型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobStatusDto {
    /// 已入池等待调度。
    Queued,
    /// 已下发且 Agent 确认在跑。
    Running,
    /// 步骤全部成功。
    Succeeded,
    /// 失败（非 allow_failure 即触发 fail-fast 级联）。
    Failed,
    /// 构建取消/同阶段失败级联。
    Cancelled,
    /// 阶段 when 不满足整级跳过 / 级联跳过后续阶段。
    Skipped,
    /// 离线不判死中间态。
    Unknown,
    /// 超时走取消路径的终态。
    Timeout,
    /// Agent 重启丢任务上报的终态。
    Aborted,
}

impl From<JobStatus> for JobStatusDto {
    fn from(s: JobStatus) -> Self {
        match s {
            JobStatus::Queued => Self::Queued,
            JobStatus::Running => Self::Running,
            JobStatus::Succeeded => Self::Succeeded,
            JobStatus::Failed => Self::Failed,
            JobStatus::Cancelled => Self::Cancelled,
            JobStatus::Skipped => Self::Skipped,
            JobStatus::Unknown => Self::Unknown,
            JobStatus::Timeout => Self::Timeout,
            JobStatus::Aborted => Self::Aborted,
        }
    }
}

/// 重跑模式（`from_scratch` 新号 / `from_failed` 同号 attempt+1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RerunModeDto {
    /// 从头重跑：新构建号、attempt=1。
    FromScratch,
    /// 从失败任务重跑：同号、attempt+1（已成功任务保留）。
    FromFailed,
}

// ---------------------------------------------------------------------------
// 请求 / 响应体
// ---------------------------------------------------------------------------

/// 手动触发请求体：参数覆盖（默认值之上叠加）+ 可选 git 分支/commit 或 svn
/// revision。空体等价于全默认（参数取定义默认值、git 缺省项目默认分支）。
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct TriggerBuildRequest {
    /// 参数覆盖（名→值；默认值之上叠加，不存在的名无害——未被引用即无效果）。
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// git 分支（手动可选；缺省项目默认分支；svn 无分支概念）。
    #[serde(default)]
    pub branch: Option<String>,
    /// git commit sha（手动未钉为空——Agent 检分支头）。
    #[serde(default)]
    pub commit: Option<String>,
    /// svn revision（手动可选；git 为空）。
    #[serde(default)]
    pub revision: Option<String>,
}

/// 重跑请求体：模式二选一。
#[derive(Debug, Deserialize, ToSchema)]
pub struct RerunBuildRequest {
    /// 重跑模式。
    pub mode: RerunModeDto,
}

/// 触发 / 重跑的受理响应：构建号 + 当前状态（异步推进，sched 驱动）。
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildAcceptedResponse {
    /// per-pipeline 构建号。
    pub number: i64,
    /// 构建行 id。
    pub build_id: i64,
    /// 重跑 attempt（首跑 1；from_failed +1）。
    pub attempt: i32,
    /// 受理时状态（queued / running）。
    pub status: BuildStatusDto,
}

/// 列表查询参数（全部可选；非法 status/分页 422）。
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListBuildsQuery {
    /// 单页条数（1..=100，默认 20）。
    pub limit: Option<i64>,
    /// 页码（从 1 起，默认 1）。
    pub page: Option<i64>,
    /// 状态过滤（queued/running/succeeded/failed/cancelled/timeout；非法 422）。
    #[param(schema_with = build_status_query_schema)]
    pub status: Option<String>,
}

/// 状态过滤参数的 OpenAPI schema：取值域与 [`BuildStatus`] 契约同源。
fn build_status_query_schema() -> utoipa::openapi::schema::Object {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .enum_values(Some(
            [
                BuildStatus::Queued,
                BuildStatus::Running,
                BuildStatus::Succeeded,
                BuildStatus::Failed,
                BuildStatus::Cancelled,
                BuildStatus::Timeout,
            ]
            .into_iter()
            .map(BuildStatus::as_str),
        ))
        .build()
}

/// 列表条目（构建概要）。
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildSummaryResponse {
    /// per-pipeline 构建号。
    pub number: i64,
    /// pipeline 名。
    pub pipeline_name: String,
    /// 构建状态。
    pub status: BuildStatusDto,
    /// 触发源。
    pub trigger: TriggerSourceDto,
    /// 触发人（业务表实名）。
    pub trigger_by: String,
    /// 重跑 attempt。
    pub attempt: i32,
    /// 开始时刻（queued→running；未运行为空）。
    pub started_at: Option<i64>,
    /// 终态时刻。
    pub finished_at: Option<i64>,
    /// 取消时刻。
    pub cancelled_at: Option<i64>,
}

/// 分页列表响应。
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildListResponse {
    /// 本页构建概要（按号倒序）。
    pub items: Vec<BuildSummaryResponse>,
    /// 总条数（满足过滤条件；供客户端算页数）。
    pub total: i64,
    /// 当前页码。
    pub page: i64,
    /// 单页条数。
    pub limit: i64,
}

/// 任务视图（构建详情内；含 attempt 历史——重跑后同任务多行并列）。
#[derive(Debug, Serialize, ToSchema)]
pub struct JobViewDto {
    /// 任务名。
    pub name: String,
    /// 任务状态。
    pub status: JobStatusDto,
    /// 第几次执行（同任务重跑 attempt+1）。
    pub attempt: i32,
    /// 开始时刻。
    pub started_at: Option<i64>,
    /// 终态时刻。
    pub finished_at: Option<i64>,
    /// 退出码（可空）。
    pub exit_code: Option<i32>,
    /// allow_failure 豁免 fail-fast。
    pub allow_failure: bool,
    /// 详情（失败原因、缺失机密名、超时等；机密只记名不泄值）。
    pub detail: Option<String>,
    /// 调度到的 Agent 行 id（未调度为空）。
    pub agent_id: Option<i64>,
}

/// 阶段视图（构建详情内；阶段名取自构建快照）。
#[derive(Debug, Serialize, ToSchema)]
pub struct StageViewDto {
    /// 阶段序号（从 0 起）。
    pub index: i32,
    /// 阶段名（快照内定义）。
    pub name: String,
    /// 阶段内任务（含 attempt 历史）。
    pub jobs: Vec<JobViewDto>,
}

/// 构建详情响应。
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildDetailResponse {
    /// per-pipeline 构建号。
    pub number: i64,
    /// pipeline 名。
    pub pipeline_name: String,
    /// 构建状态。
    pub status: BuildStatusDto,
    /// 触发源。
    pub trigger: TriggerSourceDto,
    /// 触发人。
    pub trigger_by: String,
    /// 重跑 attempt。
    pub attempt: i32,
    /// 开始时刻（queued→running）。
    pub started_at: Option<i64>,
    /// 终态时刻。
    pub finished_at: Option<i64>,
    /// 取消时刻。
    pub cancelled_at: Option<i64>,
    /// 耗时（毫秒；已完成 = finished-started，运行中 = now-started，未运行为空）。
    pub elapsed_ms: Option<i64>,
    /// 阶段与任务状态（按快照阶段序）。
    pub stages: Vec<StageViewDto>,
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// 手动触发构建（runner 档）：参数覆盖默认值 + 可选分支/commit/revision，
/// 调 engine 统一触发入口，返回 202 + 构建号。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds",
    tag = "builds",
    request_body = TriggerBuildRequest,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
    ),
    responses(
        (status = 202, description = "已触发，返回构建号（异步推进）", body = BuildAcceptedResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "viewer 档不足（触发需 runner 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或 pipeline 不存在", body = ErrorBody),
        (status = 422, description = "请求体非法", body = ErrorBody),
    )
)]
pub async fn trigger(
    State(state): State<AppState>,
    RequireRunner(access): RequireRunner,
    Path((_project, pipeline)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<BuildAcceptedResponse>), ApiError> {
    // 空体等价全默认（无覆盖、缺省分支）——触发无参数是常见形态。
    let req: TriggerBuildRequest = if body.is_empty() {
        TriggerBuildRequest::default()
    } else {
        parse_body(&body)?
    };
    let detail = TriggerDetail {
        by: access.operator.clone(),
        branch: req.branch,
        commit: req.commit,
        revision: req.revision,
        params: req
            .params
            .into_iter()
            .map(|(name, value)| ParameterOverride { name, value })
            .collect(),
    };
    let row = state
        .engine
        .start_build(StartBuildInput {
            project_name: access.project.name.clone(),
            pipeline_name: pipeline,
            trigger: TriggerSource::Manual,
            detail,
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(build_accepted(&row))))
}

/// 取消构建（runner 档，build 级）：排队中移出、运行中经通道下发 CancelBuild
/// （engine DB 迁移 + 发事件，sched 据此下发）。终态幂等 202。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/cancel",
    tag = "builds",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
    ),
    responses(
        (status = 202, description = "已受理取消（终态幂等）", body = BuildAcceptedResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "viewer 档不足（取消需 runner 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号不存在", body = ErrorBody),
    )
)]
pub async fn cancel(
    State(state): State<AppState>,
    RequireRunner(access): RequireRunner,
    Path((_project, pipeline, number)): Path<(String, String, i64)>,
) -> Result<(StatusCode, Json<BuildAcceptedResponse>), ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let cancelled = state
        .engine
        .cancel_build(build.id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("构建 #{number} 不存在")))?;
    Ok((StatusCode::ACCEPTED, Json(build_accepted(&cancelled))))
}

/// 重跑构建（runner 档）：from_scratch 新号 attempt=1 / from_failed 同号
/// attempt+1（成功任务保留）。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/rerun",
    tag = "builds",
    request_body = RerunBuildRequest,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
    ),
    responses(
        (status = 202, description = "已受理重跑，返回新构建号/attempt", body = BuildAcceptedResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "viewer 档不足（重跑需 runner 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号不存在", body = ErrorBody),
        (status = 409, description = "from_failed 要求构建处于 failed/cancelled/timeout 终态", body = ErrorBody),
        (status = 422, description = "请求体非法（mode 缺失/未知）", body = ErrorBody),
    )
)]
pub async fn rerun(
    State(state): State<AppState>,
    RequireRunner(access): RequireRunner,
    Path((_project, pipeline, number)): Path<(String, String, i64)>,
    body: Bytes,
) -> Result<(StatusCode, Json<BuildAcceptedResponse>), ApiError> {
    let req: RerunBuildRequest = parse_body(&body)?;
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    match req.mode {
        RerunModeDto::FromScratch => {
            // 从头重跑：复制原触发上下文，by 改当前用户，新号 attempt=1。
            let mut detail: TriggerDetail = serde_json::from_str(&build.trigger_detail)
                .map_err(|e| ApiError::internal("trigger detail decode", &e))?;
            detail.by = access.operator.clone();
            let row = state
                .engine
                .start_build(StartBuildInput {
                    project_name: access.project.name.clone(),
                    pipeline_name: build.pipeline_name.clone(),
                    trigger: TriggerSource::Manual,
                    detail,
                })
                .await?;
            Ok((StatusCode::ACCEPTED, Json(build_accepted(&row))))
        }
        RerunModeDto::FromFailed => {
            // 从失败任务重跑：同号 attempt+1（成功任务保留），再 drive 重开失败任务。
            let rerun = BuildRepo::new(state.pool.clone())
                .rerun_from_failed(build.id)
                .await?
                .ok_or_else(|| {
                    ApiError::conflict(format!(
                        "构建 #{number} 当前状态不可从失败重跑（仅 failed/cancelled/timeout 终态可重跑）"
                    ))
                })?;
            state.engine.drive(rerun.id).await?;
            let row = BuildRepo::new(state.pool.clone())
                .get(rerun.id)
                .await?
                .expect("刚重跑的构建必存在");
            Ok((StatusCode::ACCEPTED, Json(build_accepted(&row))))
        }
    }
}

/// 构建列表（viewer 档）：按号倒序 + 分页 + 状态过滤。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds",
    tag = "builds",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ListBuildsQuery,
    ),
    responses(
        (status = 200, description = "构建概要列表（按号倒序）", body = BuildListResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（列表需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见", body = ErrorBody),
        (status = 422, description = "分页/状态过滤参数非法", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline)): Path<(String, String)>,
    Query(query): Query<ListBuildsQuery>,
) -> Result<Json<BuildListResponse>, ApiError> {
    let (limit, page) = parse_paging(&query)?;
    let status = parse_status_filter(&query)?;
    let offset = (page - 1) * limit;
    let repo = BuildRepo::new(state.pool.clone());
    let rows = repo
        .list_page(access.project.id, &pipeline, status, limit, offset)
        .await?;
    let total = repo
        .count_by_project(access.project.id, &pipeline, status)
        .await?;
    let items = rows.iter().map(build_summary).collect();
    Ok(Json(BuildListResponse {
        items,
        total,
        page,
        limit,
    }))
}

/// 构建详情（viewer 档）：状态/触发人/attempt/耗时/阶段与任务状态。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}",
    tag = "builds",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
    ),
    responses(
        (status = 200, description = "构建详情（含阶段与任务状态）", body = BuildDetailResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（详情需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号不存在", body = ErrorBody),
    )
)]
pub async fn detail(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number)): Path<(String, String, i64)>,
) -> Result<Json<BuildDetailResponse>, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let jobs = JobRepo::new(state.pool.clone())
        .list_by_build(build.id)
        .await?;
    let now = now_ms();
    let elapsed_ms = elapsed(&build, now);
    let stages = stage_views(&build, &jobs);
    Ok(Json(BuildDetailResponse {
        number: build.number,
        pipeline_name: build.pipeline_name.clone(),
        status: build.status.into(),
        trigger: build.trigger.into(),
        trigger_by: trigger_by(&build),
        attempt: build.attempt,
        started_at: build.started_at,
        finished_at: build.finished_at,
        cancelled_at: build.cancelled_at,
        elapsed_ms,
        stages,
    }))
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// 按 (project, pipeline, number) 取构建；不存在 404（runner/viewer 已由
/// extractor 裁决项目可见性，构建号不存在是第二层 404）。构建号 per-pipeline，
/// 须带 pipeline_name 唯一定位（同号跨 pipeline 不串）。
async fn load_build(
    state: &AppState,
    project_id: &i64,
    pipeline: &str,
    number: i64,
) -> Result<BuildRow, ApiError> {
    BuildRepo::new(state.pool.clone())
        .get_by_number(*project_id, pipeline, number)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("构建 #{number} 不存在")))
}

/// 受理响应：从构建行组装（触发/重跑共用）。
fn build_accepted(row: &BuildRow) -> BuildAcceptedResponse {
    BuildAcceptedResponse {
        number: row.number,
        build_id: row.id,
        attempt: row.attempt,
        status: row.status.into(),
    }
}

/// 列表概要：从构建行组装（trigger_by 解析自触发上下文，损坏则空串）。
fn build_summary(row: &BuildRow) -> BuildSummaryResponse {
    BuildSummaryResponse {
        number: row.number,
        pipeline_name: row.pipeline_name.clone(),
        status: row.status.into(),
        trigger: row.trigger.into(),
        trigger_by: trigger_by(row),
        attempt: row.attempt,
        started_at: row.started_at,
        finished_at: row.finished_at,
        cancelled_at: row.cancelled_at,
    }
}

/// 触发人：解析 builds.trigger_detail 的 TriggerDetail.by（损坏按空串，不 500）。
fn trigger_by(row: &BuildRow) -> String {
    serde_json::from_str::<TriggerDetail>(&row.trigger_detail)
        .map(|d| d.by)
        .unwrap_or_default()
}

/// 耗时（毫秒）：已完成 = finished-started；运行中 = now-started；未运行 None。
fn elapsed(row: &BuildRow, now: i64) -> Option<i64> {
    match (row.started_at, row.finished_at) {
        (Some(start), Some(end)) => Some(end - start),
        (Some(start), None) => Some(now - start),
        (None, _) => None,
    }
}

/// 阶段视图：按构建快照的阶段序，逐阶段挂其任务行（含 attempt 历史）。
fn stage_views(build: &BuildRow, jobs: &[crate::store::jobs::JobRow]) -> Vec<StageViewDto> {
    let snapshot: BuildSnapshot = match serde_json::from_str(&build.snapshot) {
        Ok(s) => s,
        Err(_) => {
            // 快照损坏：退化为按 stage_index 聚合、阶段名为空（不 500，详情仍可读）。
            return stages_by_index(jobs);
        }
    };
    snapshot
        .pipeline
        .stages
        .iter()
        .enumerate()
        .map(|(index, stage)| StageViewDto {
            index: index as i32,
            name: stage.name.clone(),
            jobs: jobs
                .iter()
                .filter(|j| j.stage_index == index as i32)
                .map(job_view)
                .collect(),
        })
        .collect()
}

/// 快照损坏时的退化聚合：按 stage_index 分组、阶段名空。
fn stages_by_index(jobs: &[crate::store::jobs::JobRow]) -> Vec<StageViewDto> {
    let mut indices: Vec<i32> = jobs.iter().map(|j| j.stage_index).collect();
    indices.sort_unstable();
    indices.dedup();
    indices
        .into_iter()
        .map(|index| StageViewDto {
            index,
            name: String::new(),
            jobs: jobs
                .iter()
                .filter(|j| j.stage_index == index)
                .map(job_view)
                .collect(),
        })
        .collect()
}

/// 任务行 → 任务视图。
fn job_view(j: &crate::store::jobs::JobRow) -> JobViewDto {
    JobViewDto {
        name: j.name.clone(),
        status: j.status.into(),
        attempt: j.attempt,
        started_at: j.started_at,
        finished_at: j.finished_at,
        exit_code: j.exit_code,
        allow_failure: j.allow_failure,
        detail: j.detail.clone(),
        agent_id: j.agent_id,
    }
}

// ---------------------------------------------------------------------------
// 参数解析（与 audit 同纪律：非法 422，不静默放宽）
// ---------------------------------------------------------------------------

/// 分页解析：limit 默认 20、1..=100；page 默认 1、>= 1。非法即 422。
fn parse_paging(query: &ListBuildsQuery) -> Result<(i64, i64), ApiError> {
    let mut issues = Vec::new();
    let limit = query.limit.unwrap_or(BUILDS_PAGE_DEFAULT);
    if !(1..=BUILDS_PAGE_MAX).contains(&limit) {
        issues.push(ValidationIssue {
            path: "limit".into(),
            message: format!("limit 须在 1..={BUILDS_PAGE_MAX} 之间"),
        });
    }
    let page = query.page.unwrap_or(BUILDS_PAGE_NUMBER_DEFAULT);
    if page < 1 {
        issues.push(ValidationIssue {
            path: "page".into(),
            message: "page 须 >= 1".into(),
        });
    }
    if issues.is_empty() {
        Ok((limit, page))
    } else {
        Err(ApiError::validation("构建分页参数非法", issues))
    }
}

/// 状态过滤解析：未知值 422（取值域与 BuildStatus 契约同源）。
fn parse_status_filter(query: &ListBuildsQuery) -> Result<Option<BuildStatus>, ApiError> {
    match &query.status {
        Some(raw) => match BuildStatus::parse(raw) {
            Ok(status) => Ok(Some(status)),
            Err(_) => Err(ApiError::validation(
                "构建状态过滤参数非法",
                vec![ValidationIssue {
                    path: "status".into(),
                    message: format!(
                        "未知状态：{raw}（取值：queued/running/succeeded/failed/cancelled/timeout）"
                    ),
                }],
            )),
        },
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paging_defaults_and_bounds() {
        assert_eq!(
            parse_paging(&ListBuildsQuery::default()).expect("默认"),
            (BUILDS_PAGE_DEFAULT, BUILDS_PAGE_NUMBER_DEFAULT)
        );
        assert_eq!(
            parse_paging(&ListBuildsQuery {
                limit: Some(5),
                page: Some(3),
                status: None,
            })
            .expect("自定义"),
            (5, 3)
        );
        // limit 越界 / page < 1 → 422。
        for (limit, page) in [(0, 1), (BUILDS_PAGE_MAX + 1, 1), (10, 0)] {
            let err = parse_paging(&ListBuildsQuery {
                limit: Some(limit),
                page: Some(page),
                status: None,
            })
            .unwrap_err();
            assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn parse_status_filter_accepts_contract_values_and_rejects_unknown() {
        for raw in ["queued", "running", "succeeded", "failed", "cancelled", "timeout"] {
            let q = ListBuildsQuery {
                status: Some(raw.into()),
                ..Default::default()
            };
            assert!(parse_status_filter(&q).is_ok(), "{raw} 应可解析");
        }
        let err = parse_status_filter(&ListBuildsQuery {
            status: Some("bogus".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(parse_status_filter(&ListBuildsQuery::default()).expect("空").is_none());
    }

    #[test]
    fn elapsed_is_finished_running_or_none() {
        let mut row = BuildRow {
            id: 1,
            project_id: 1,
            pipeline_name: "release".into(),
            number: 1,
            status: BuildStatus::Running,
            trigger: TriggerSource::Manual,
            trigger_detail: "{}".into(),
            attempt: 1,
            snapshot: "{}".into(),
            started_at: None,
            finished_at: None,
            cancelled_at: None,
            updated_at: 0,
        };
        assert_eq!(elapsed(&row, 100), None, "未运行无耗时");
        row.started_at = Some(10);
        assert_eq!(elapsed(&row, 100), Some(90), "运行中 = now-started");
        row.finished_at = Some(55);
        assert_eq!(elapsed(&row, 100), Some(45), "已完成 = finished-started");
    }
}

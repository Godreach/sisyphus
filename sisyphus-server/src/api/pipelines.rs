//! Pipeline 定义端点（票 B2a-T4；B2b-T5 授权 retrofit）：GET viewer 档
//! （定义 + revision + 操作人/时间）与 PUT 项目 admin 档（model 校验失败
//! 422 + 错误清单整组透传；成功返回新 revision，操作人为认证用户实名）。
//!
//! 定义以 sisyphus-model 的 JSON 形态往返、原样落库读回（schema 不解析
//! 定义内部，ADR-0009）；OpenAPI 侧 schema 事实源在 model，此处声明为
//! 自由 object，TS 类型随后续批次从 model 生成。项目存在性与档位由
//! [`super::policy`] extractor 先行裁决（无角色 404 / 档位不足 403）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde::Serialize;
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::builds::{BuildStatusDto, TriggerSourceDto};
use super::error::{ApiError, ErrorBody, parse_body};
use super::policy::{RequireAdmin, RequireViewer};
use crate::store::builds::{BuildRepo, BuildRow, BuildStatus};
use crate::store::pipelines::PipelineListItem;
use sisyphus_model::pipeline::Pipeline;

/// PUT 请求体：Pipeline 定义（sisyphus-model JSON 形态）。
///
/// OpenAPI 契约里是自由 object——schema 事实源在 sisyphus-model，
/// 不在 API 层复刻一份漂移源。
#[derive(Debug, Serialize, ToSchema)]
#[schema(value_type = Object)]
pub struct PipelineDefinitionPayload(pub serde_json::Value);

/// GET 响应：当前定义 + 修订版本语义字段。
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineDefinitionResponse {
    /// Pipeline 定义（model JSON 形态，与提交等价读回）。
    pub definition: PipelineDefinitionPayload,
    /// 当前修订版本号（每次保存 +1，从 1 起）。
    pub revision: u32,
    /// 最后保存的操作人。
    pub operator: String,
    /// 最后保存时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// PUT 响应：保存成功，返回新修订版本。
#[derive(Debug, Serialize, ToSchema)]
pub struct SaveDefinitionResponse {
    /// 本次保存后的修订版本号。
    pub revision: u32,
    /// 操作人（登录用户名，票 B2b-T1 起）。
    pub operator: String,
    /// 保存时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 跨项目流水线清单项（viewer 可见范围内）。
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineListItemResponse {
    /// 所属项目名。
    pub project: String,
    /// 流水线名。
    pub pipeline: String,
    /// 定义最近修改时间（Unix 毫秒）。
    pub updated_at: i64,
}

impl From<PipelineListItem> for PipelineListItemResponse {
    fn from(item: PipelineListItem) -> Self {
        Self {
            project: item.project,
            pipeline: item.pipeline,
            updated_at: item.updated_at,
        }
    }
}

/// 跨项目流水线清单响应。
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineListResponse {
    /// 按项目名、流水线名字典序排列的清单。
    pub items: Vec<PipelineListItemResponse>,
    /// 清单总数（等于 `items.length`）。
    pub total: i64,
}

/// 统计查询参数。窗口值由 handler 统一钳制，保持与前端 mock/构建列表
/// 的口径一致：缺省或无法解析时取 20，合法域为 1..=100。
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct PipelineStatsQuery {
    /// 最近构建数窗口（缺省 20，最终钳制到 1..=100）。
    pub window: Option<String>,
}

/// 最近一条构建概要（任意状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LatestBuildResponse {
    /// per-pipeline 构建号。
    pub number: i64,
    /// 构建状态。
    pub status: BuildStatusDto,
    /// 触发源。
    pub trigger: TriggerSourceDto,
    /// 开始时刻（未运行为空）。
    pub started_at: Option<i64>,
    /// 终态时刻。
    pub finished_at: Option<i64>,
}

/// 流水线统计响应（窗口按构建号倒序；成功率分母为窗口内终态）。
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineStatsResponse {
    /// 服务端实际采用的窗口大小。
    pub window: i64,
    /// 该流水线的全部构建数。
    pub total_builds: i64,
    /// 窗口内终态构建数。
    pub terminal_count: i64,
    /// 窗口内 succeeded 构建数。
    pub succeeded_count: i64,
    /// 成功率（百分比，一位小数；无终态时为空）。
    pub success_rate: Option<f64>,
    /// 平均耗时（毫秒；无可测样本时为空）。
    pub avg_duration_ms: Option<i64>,
    /// 最近一条构建（从未构建时为空）。
    pub latest_build: Option<LatestBuildResponse>,
}

const STATS_WINDOW_DEFAULT: i64 = 20;
const STATS_WINDOW_MAX: i64 = 100;

/// 解析并钳制统计窗口。保留 mock 契约的容错语义：非数值/非有限值回退
/// 缺省值；小数向下取整；越界值收敛到边界。
fn parse_stats_window(raw: Option<&str>) -> i64 {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return STATS_WINDOW_DEFAULT;
    };
    let Ok(value) = raw.trim().parse::<f64>() else {
        return STATS_WINDOW_DEFAULT;
    };
    if !value.is_finite() {
        return STATS_WINDOW_DEFAULT;
    }
    (value.floor() as i64).clamp(1, STATS_WINDOW_MAX)
}

fn pipeline_stats_from_rows(
    rows: &[BuildRow],
    total_builds: i64,
    requested_window: i64,
) -> PipelineStatsResponse {
    let window = requested_window
        .clamp(1, STATS_WINDOW_MAX)
        .min(total_builds.max(0));
    let in_window = rows.iter().take(window as usize);
    let mut terminal_count = 0_i64;
    let mut succeeded_count = 0_i64;
    let mut durations = Vec::new();

    for row in in_window {
        if !row.status.is_terminal() {
            continue;
        }
        terminal_count += 1;
        if row.status == BuildStatus::Succeeded {
            succeeded_count += 1;
        }
        if let (Some(started), Some(finished)) = (row.started_at, row.finished_at) {
            durations.push(finished - started);
        }
    }

    let success_rate = (terminal_count > 0)
        .then(|| ((succeeded_count as f64 / terminal_count as f64) * 1000.0).round() / 10.0);
    let avg_duration_ms = if durations.is_empty() {
        None
    } else {
        let sum: i128 = durations.iter().map(|duration| *duration as i128).sum();
        Some((sum as f64 / durations.len() as f64).round() as i64)
    };
    let latest_build = rows.first().map(|row| LatestBuildResponse {
        number: row.number,
        status: row.status.into(),
        trigger: row.trigger.into(),
        started_at: row.started_at,
        finished_at: row.finished_at,
    });

    PipelineStatsResponse {
        window,
        total_builds,
        terminal_count,
        succeeded_count,
        success_rate,
        avg_duration_ms,
        latest_build,
    }
}

/// 列出当前调用者可见的全部流水线（全局管理员为全部活动项目，普通用户
/// 为具备项目角色的项目；无可见项目返回空清单）。
#[utoipa::path(
    get,
    path = "/api/v1/pipelines",
    tag = "pipelines",
    responses(
        (status = 200, description = "调用者可见的跨项目流水线（按项目名、流水线名排序）", body = PipelineListResponse),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<PipelineListResponse>, ApiError> {
    let items = state
        .pipelines
        .list_visible(auth.is_admin, auth.user_id)
        .await?
        .into_iter()
        .map(Into::into)
        .collect::<Vec<PipelineListItemResponse>>();
    Ok(Json(PipelineListResponse {
        total: items.len() as i64,
        items,
    }))
}

/// 读取单条流水线统计（viewer 档）：统计窗口与构建列表同源，按构建号
/// 倒序取最近 N 条；流水线不存在时返回 404，不把空列表误当作存在。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/stats",
    tag = "pipelines",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        PipelineStatsQuery,
    ),
    responses(
        (status = 200, description = "流水线统计", body = PipelineStatsResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足", body = ErrorBody),
        (status = 404, description = "项目或 pipeline 不存在（或项目不可见）", body = ErrorBody),
    )
)]
pub async fn stats(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_name, pipeline)): Path<(String, String)>,
    Query(query): Query<PipelineStatsQuery>,
) -> Result<Json<PipelineStatsResponse>, ApiError> {
    // RequireViewer 只裁决项目可见性；pipeline 不存在必须单独裁决，避免
    // 与构建列表的空列表形态混淆。
    if state
        .pipelines
        .get(&access.project.name, &pipeline)
        .await?
        .is_none()
    {
        return Err(ApiError::resource_not_found(format!(
            "pipeline {}/{} 不存在",
            access.project.name, pipeline
        )));
    }

    let requested_window = parse_stats_window(query.window.as_deref());
    let builds = BuildRepo::new(state.pool.clone());
    let total_builds = builds
        .count_by_project(access.project.id, &pipeline, None)
        .await?;
    // total=0 时仍传 1，聚合函数会将实际窗口收敛为 0；避免 SQL LIMIT 0
    // 导致边界值与响应不一致。
    let rows = builds
        .list_page(
            access.project.id,
            &pipeline,
            None,
            requested_window.clamp(1, STATS_WINDOW_MAX),
            0,
        )
        .await?;
    Ok(Json(pipeline_stats_from_rows(
        &rows,
        total_builds,
        requested_window,
    )))
}

/// 读 pipeline 定义（viewer 档：无角色与项目不存在同形 404，票 B2b-T5）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}",
    tag = "pipelines",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
    ),
    responses(
        (status = 200, description = "当前定义与修订版本", body = PipelineDefinitionResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "项目或 pipeline 不存在（或项目不可见）", body = ErrorBody),
    )
)]
pub async fn get_definition(
    State(state): State<AppState>,
    RequireViewer(_access): RequireViewer,
    Path((name, pipeline)): Path<(String, String)>,
) -> Result<Json<PipelineDefinitionResponse>, ApiError> {
    let stored = state
        .pipelines
        .get(&name, &pipeline)
        .await?
        .ok_or_else(|| {
            ApiError::resource_not_found(format!("pipeline {name}/{pipeline} 不存在"))
        })?;
    let definition: serde_json::Value = serde_json::from_str(&stored.definition)
        .map_err(|e| ApiError::internal("definition decode", &e))?;
    Ok(Json(PipelineDefinitionResponse {
        definition: PipelineDefinitionPayload(definition),
        revision: stored.revision,
        operator: stored.operator,
        updated_at: stored.updated_at,
    }))
}

/// 保存 pipeline 定义（项目 admin 档，票 B2b-T5；upsert：首存 revision=1，
/// 续存 +1；可选 If-None-Match: * 原子首建，已存在返回 412，票 #120）。
#[utoipa::path(
    put,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}",
    tag = "pipelines",
    request_body = PipelineDefinitionPayload,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("If-None-Match" = Option<String>, Header, description = "首次创建时传 *，原子保证不存在；同名已存在返回 412 且不覆盖。普通编辑不传此头。"),
    ),
    responses(
        (status = 200, description = "已保存，返回新修订版本", body = SaveDefinitionResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（保存定义需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见（不泄露存在性）", body = ErrorBody),
        (status = 422, description = "model 校验失败，错误清单整组透传", body = ErrorBody),
        (status = 412, description = "条件首建失败：同项目同名流水线已存在", body = ErrorBody),
    )
)]
pub async fn put_definition(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<super::auth::AuthContext>,
    RequireAdmin(access): RequireAdmin,
    Path((_project_name, pipeline)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SaveDefinitionResponse>, ApiError> {
    // 先落 model 类型：形态错也是校验失败（统一 422 形态，不走 axum 默认拒绝）。
    let definition: Pipeline = parse_body(&body)?;
    // 操作人实名：认证中间件注入的登录用户名（票 B2b-T1）。
    // `If-None-Match: *` 是新建态唯一允许的条件首建语义。普通 PUT 不带该头，
    // 继续沿用历史 upsert 行为，保证已有编辑器/脚本兼容。
    let revision = if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "*")
    {
        state
            .pipelines
            .create_if_absent(&access.project.name, &pipeline, &definition, &auth.username)
            .await?
    } else {
        state
            .pipelines
            .save(&access.project.name, &pipeline, &definition, &auth.username)
            .await?
    };
    Ok(Json(SaveDefinitionResponse {
        revision: revision.number,
        operator: revision.operator,
        updated_at: revision.at_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        number: i64,
        status: BuildStatus,
        started_at: Option<i64>,
        finished_at: Option<i64>,
    ) -> BuildRow {
        BuildRow {
            id: number,
            project_id: 1,
            pipeline_name: "main".into(),
            number,
            status,
            trigger: crate::store::builds::TriggerSource::Manual,
            trigger_detail: r#"{"by":"tester"}"#.into(),
            attempt: 1,
            snapshot: "{}".into(),
            started_at,
            finished_at,
            cancelled_at: None,
            updated_at: finished_at.or(started_at).unwrap_or(0),
        }
    }

    #[test]
    fn stats_window_defaults_and_clamps_like_frontend_contract() {
        assert_eq!(parse_stats_window(None), 20);
        assert_eq!(parse_stats_window(Some("")), 20);
        assert_eq!(parse_stats_window(Some("nope")), 20);
        assert_eq!(parse_stats_window(Some("1.9")), 1);
        assert_eq!(parse_stats_window(Some("0")), 1);
        assert_eq!(parse_stats_window(Some("999")), 100);
    }

    #[test]
    fn stats_use_terminal_only_and_keep_latest_any_state() {
        let rows = vec![
            row(3, BuildStatus::Running, Some(300), None),
            row(2, BuildStatus::Succeeded, Some(100), Some(300)),
            row(1, BuildStatus::Failed, Some(0), Some(100)),
        ];
        let stats = pipeline_stats_from_rows(&rows, 3, 2);
        assert_eq!(stats.window, 2);
        assert_eq!(stats.total_builds, 3);
        assert_eq!(stats.terminal_count, 1);
        assert_eq!(stats.succeeded_count, 1);
        assert_eq!(stats.success_rate, Some(100.0));
        assert_eq!(stats.avg_duration_ms, Some(200));
        assert_eq!(
            stats.latest_build.as_ref().map(|build| build.number),
            Some(3)
        );
        assert_eq!(
            stats.latest_build.as_ref().map(|build| build.status),
            Some(BuildStatusDto::Running)
        );
    }

    #[test]
    fn stats_return_nulls_when_no_builds_or_no_measurable_duration() {
        let stats = pipeline_stats_from_rows(&[], 0, 20);
        assert_eq!(stats.window, 0);
        assert_eq!(stats.success_rate, None);
        assert_eq!(stats.avg_duration_ms, None);
        assert_eq!(stats.latest_build, None);

        let rows = vec![row(1, BuildStatus::Cancelled, None, Some(100))];
        let stats = pipeline_stats_from_rows(&rows, 1, 20);
        assert_eq!(stats.terminal_count, 1);
        assert_eq!(stats.success_rate, Some(0.0));
        assert_eq!(stats.avg_duration_ms, None);
    }
}

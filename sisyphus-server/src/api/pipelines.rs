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
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, parse_body};
use super::policy::{RequireAdmin, RequireViewer};
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
/// 续存 +1；操作人为认证用户实名，票 B2b-T1）。
#[utoipa::path(
    put,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}",
    tag = "pipelines",
    request_body = PipelineDefinitionPayload,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
    ),
    responses(
        (status = 200, description = "已保存，返回新修订版本", body = SaveDefinitionResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（保存定义需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见（不泄露存在性）", body = ErrorBody),
        (status = 422, description = "model 校验失败，错误清单整组透传", body = ErrorBody),
    )
)]
pub async fn put_definition(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<super::auth::AuthContext>,
    RequireAdmin(access): RequireAdmin,
    Path((_project_name, pipeline)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<SaveDefinitionResponse>, ApiError> {
    // 先落 model 类型：形态错也是校验失败（统一 422 形态，不走 axum 默认拒绝）。
    let definition: Pipeline = parse_body(&body)?;
    // 操作人实名：认证中间件注入的登录用户名（票 B2b-T1）。
    let revision = state
        .pipelines
        .save(&access.project.name, &pipeline, &definition, &auth.username)
        .await?;
    Ok(Json(SaveDefinitionResponse {
        revision: revision.number,
        operator: revision.operator,
        updated_at: revision.at_ms,
    }))
}

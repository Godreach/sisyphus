//! 项目端点（票 B2a-T4；B2b-T5 授权 retrofit）：list（按可见性过滤）/
//! create（全局 admin）/ get（viewer 档）。
//!
//! update/delete 及其级联语义（pipeline 删除对构建历史的影响）归后续批次
//! 裁定，不预开端点。认证（401）由 `/api/v1` 全局中间件统一把关；项目级
//! 授权（404/403）由 [`super::policy`] 的端点 extractor 声明（矩阵本体在
//! [`crate::auth`]，票 B2b-T5）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireViewer;
use crate::store::projects::{NewProject, Project, ScmType};

/// 仓库类型（API 形态；`git` / `svn`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScmTypeDto {
    /// git 仓库（默认分支可空）。
    Git,
    /// svn 仓库（URL 即唯一监控对象，无分支概念）。
    Svn,
}

impl From<ScmTypeDto> for ScmType {
    fn from(dto: ScmTypeDto) -> Self {
        match dto {
            ScmTypeDto::Git => Self::Git,
            ScmTypeDto::Svn => Self::Svn,
        }
    }
}

impl From<ScmType> for ScmTypeDto {
    fn from(domain: ScmType) -> Self {
        match domain {
            ScmType::Git => Self::Git,
            ScmType::Svn => Self::Svn,
        }
    }
}

/// 创建项目请求体。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    /// 项目名（唯一）。
    pub name: String,
    /// 仓库类型。
    pub scm_type: ScmTypeDto,
    /// 仓库 URL。
    pub scm_url: String,
    /// git 默认分支（可空；svn 项目不适用）。
    pub default_branch: Option<String>,
}

/// 项目视图（list / create / get 共用）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectResponse {
    /// 行 id。
    pub id: i64,
    /// 项目名（唯一）。
    pub name: String,
    /// 仓库类型（`git` / `svn`）。
    pub scm_type: ScmTypeDto,
    /// 仓库 URL。
    pub scm_url: String,
    /// git 默认分支（可空；svn 恒空）。
    pub default_branch: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

impl From<Project> for ProjectResponse {
    fn from(p: Project) -> Self {
        Self {
            id: p.id,
            name: p.name,
            scm_type: p.scm_type.into(),
            scm_url: p.scm_url,
            default_branch: p.default_branch,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

/// 项目清单（按可见性过滤：全局 admin 全量、普通用户只列有角色的项目，
/// 票 B2b-T5）。
#[utoipa::path(
    get,
    path = "/api/v1/projects",
    tag = "projects",
    responses(
        (status = 200, description = "调用者可见的项目（全局 admin 全量、普通用户仅有角色的项目；按名排序）", body = [ProjectResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<ProjectResponse>>, ApiError> {
    let projects = state
        .projects
        .list_visible(auth.is_admin, auth.user_id)
        .await?;
    Ok(Json(projects.into_iter().map(Into::into).collect()))
}

/// 创建项目（全局管理员专属，票 B2b-T5：全局资源只认 `is_admin`）。
#[utoipa::path(
    post,
    path = "/api/v1/projects",
    tag = "projects",
    request_body = CreateProjectRequest,
    responses(
        (status = 201, description = "已创建", body = ProjectResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员（建项目为全局资源）", body = ErrorBody),
        (status = 409, description = "项目名已存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（错误清单整组透传）", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Bytes,
) -> Result<(StatusCode, Json<ProjectResponse>), ApiError> {
    if !auth.is_admin {
        return Err(ApiError::forbidden("创建项目为全局管理员专属操作"));
    }
    let req: CreateProjectRequest = parse_body(&body)?;
    let issues = validate_create(&req);
    if !issues.is_empty() {
        return Err(ApiError::validation("项目输入校验失败", issues));
    }

    let project = state
        .projects
        .create(NewProject {
            name: req.name.trim().to_string(),
            scm_type: req.scm_type.into(),
            scm_url: req.scm_url.trim().to_string(),
            default_branch: req
                .default_branch
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty()),
        })
        .await?;
    // 审计（票 B2b-T7，ADR-0015）：项目建——项目域事件记项目名（审计
    // 保留名不保留引用，项目行随未来批次删除也不悬空）。
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            crate::store::audit::AuditEvent::ProjectCreated,
            Some(&project.name),
            None,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(project.into())))
}

/// 按名取项目（viewer 档声明：无角色与不存在同形 404，票 B2b-T5）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}",
    tag = "projects",
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 200, description = "项目", body = ProjectResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "项目不存在或对调用者不可见（不泄露存在性）", body = ErrorBody),
    )
)]
pub async fn get_one(
    RequireViewer(access): RequireViewer,
) -> Result<Json<ProjectResponse>, ApiError> {
    Ok(Json(access.project.into()))
}

/// 创建项目的字段校验（轻量输入面；pipeline 定义的重校验在 model 单一事实源）。
fn validate_create(req: &CreateProjectRequest) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if req.name.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "name".into(),
            message: "项目名不能为空".into(),
        });
    }
    if req.scm_url.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "scm_url".into(),
            message: "仓库 URL 不能为空".into(),
        });
    }
    if req.scm_type == ScmTypeDto::Svn && req.default_branch.is_some() {
        issues.push(ValidationIssue {
            path: "default_branch".into(),
            message: "svn 项目无分支概念，不支持默认分支".into(),
        });
    }
    issues
}

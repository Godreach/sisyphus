//! 项目端点（票 B2a-T4；B2b-T5 授权 retrofit）：list（按可见性过滤）/
//! create（全局 admin）/ get（viewer 档）/ update（项目 admin 档）。
//!
//! delete 及其级联语义（pipeline 删除对构建历史的影响）由删除批次承载。
//! 认证（401）由 `/api/v1` 全局中间件统一把关；项目级
//! 授权（404/403）由 [`super::policy`] 的端点 extractor 声明（矩阵本体在
//! [`crate::auth`]，票 B2b-T5）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::{RequireAdmin, RequireGlobalAdmin, RequireViewer};
use crate::store::projects::{NewProject, Project, ScmType, UpdateProject};

/// 仓库类型（API 形态；`git` / `svn` / `none`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScmTypeDto {
    /// git 仓库（默认分支可空）。
    Git,
    /// svn 仓库（URL 即唯一监控对象，无分支概念）。
    Svn,
    /// 不绑定版本管理器的空工作区项目。
    None,
}

impl From<ScmTypeDto> for ScmType {
    fn from(dto: ScmTypeDto) -> Self {
        match dto {
            ScmTypeDto::Git => Self::Git,
            ScmTypeDto::Svn => Self::Svn,
            ScmTypeDto::None => Self::None,
        }
    }
}

impl From<ScmType> for ScmTypeDto {
    fn from(domain: ScmType) -> Self {
        match domain {
            ScmType::Git => Self::Git,
            ScmType::Svn => Self::Svn,
            ScmType::None => Self::None,
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
    /// 仓库 URL；`none` 类型传空字符串。
    pub scm_url: String,
    /// git 默认分支（可空；svn 项目不适用）。
    pub default_branch: Option<String>,
    /// 可选 SCM 用户名（与 password 一并加密落库，供 poll/测试连接探测用；
    /// B5-T3，ADR-0015/0016）。值只写不读，任何端点不回显。
    #[serde(default)]
    pub scm_username: Option<String>,
    /// 可选 SCM 密码/token（加密落库；永不上命令行/URL）。
    #[serde(default)]
    pub scm_password: Option<String>,
}

/// 编辑项目请求体。字段缺省保持原值；`default_branch: null` 清除默认分支。
/// `scm_url` 只能以非空 `http://` / `https://` URL 替换，项目名不可改。
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    /// 新仓库 URL；缺省不变。
    pub scm_url: Option<String>,
    /// 默认分支三态值：缺省不变、null 清除、字符串替换。
    #[serde(default, deserialize_with = "deserialize_patch_nullable")]
    pub default_branch: Option<Option<String>>,
}

/// Preserve the distinction between an omitted nullable PATCH field (`None`)
/// and an explicit JSON null (`Some(None)`).
fn deserialize_patch_nullable<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

/// 项目视图（list / create / get 共用）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectResponse {
    /// 行 id。
    pub id: i64,
    /// 项目名（唯一）。
    pub name: String,
    /// 仓库类型（`git` / `svn` / `none`）。
    pub scm_type: ScmTypeDto,
    /// 仓库 URL；`none` 类型为空字符串。
    pub scm_url: String,
    /// git 默认分支（可空；svn 恒空）。
    pub default_branch: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
    /// 项目下的流水线定义数量。
    pub pipeline_count: i64,
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
            pipeline_count: p.pipeline_count,
        }
    }
}

/// 项目清单权限过滤值。
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ListProjectsPermission {
    /// 仅返回调用者可管理的项目。
    Admin,
}

/// 项目清单可选过滤。
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ListProjectsQuery {
    /// `admin` 时仅返回调用者可管理的项目；缺省保持既有可见性语义。
    pub permission: Option<ListProjectsPermission>,
}

/// 项目清单（按可见性过滤：全局 admin 全量、普通用户只列有角色的项目，
/// 票 B2b-T5）。
#[utoipa::path(
    get,
    path = "/api/v1/projects",
    tag = "projects",
    params(("permission" = Option<ListProjectsPermission>, Query, description = "可选权限过滤；admin 仅返回可管理项目")),
    responses(
        (status = 200, description = "调用者可见的项目（全局 admin 全量、普通用户仅有角色的项目；按名排序）", body = [ProjectResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(query): Query<ListProjectsQuery>,
) -> Result<Json<Vec<ProjectResponse>>, ApiError> {
    let projects = match query.permission {
        None => {
            state
                .projects
                .list_visible(auth.is_admin, auth.user_id)
                .await?
        }
        Some(ListProjectsPermission::Admin) => {
            state
                .projects
                .list_manageable(auth.is_admin, auth.user_id)
                .await?
        }
    };
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

    // SCM 凭据（可选，B5-T3）：先取出（加密落库在项目创建后，凭 project.id）。
    // 用户名 trim、密码原样（密码可含首尾空白）；空串视为不设。
    let scm_username = req
        .scm_username
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let scm_password = req.scm_password.filter(|s| !s.is_empty());

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
    // SCM 凭据落库（加密，复用机密同套 ADR-0015）+ 审计 set（永不记值）。
    let scm_ciphertext = match scm_password.as_deref() {
        Some(p) => Some(
            crate::secrets::encrypt(&state.master_key, p.as_bytes())
                .map_err(|e| ApiError::internal("scm credential encrypt", &e))?,
        ),
        None => None,
    };
    if scm_username.is_some() || scm_ciphertext.is_some() {
        state
            .scm_credentials
            .set(
                project.id,
                scm_username.as_deref(),
                scm_ciphertext.as_deref(),
                &auth.username,
                crate::store::now_ms(),
            )
            .await?;
        state
            .audit
            .insert(
                crate::store::now_ms(),
                &auth.username,
                crate::store::audit::AuditEvent::ScmCredentialSet,
                Some(&project.name),
                Some(&serde_json::json!({ "action": "set" }).to_string()),
            )
            .await?;
    }
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

/// 更新项目设置（项目 admin；PATCH 语义，票 #136）。
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{name}",
    tag = "projects",
    params(("name" = String, Path, description = "项目名")),
    request_body = UpdateProjectRequest,
    responses(
        (status = 200, description = "已更新的项目（含最新更新时间）", body = ProjectResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "调用者不是项目管理员", body = ErrorBody),
        (status = 404, description = "项目不存在或对调用者不可见", body = ErrorBody),
        (status = 422, description = "SCM URL 或默认分支校验失败", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    body: Bytes,
) -> Result<Json<ProjectResponse>, ApiError> {
    let req: UpdateProjectRequest = parse_body(&body)?;
    let issues = validate_update(&access.project, &req);
    if !issues.is_empty() {
        return Err(ApiError::validation("项目输入校验失败", issues));
    }

    let scm_url = req.scm_url.map(|url| url.trim().to_string());
    let default_branch = req.default_branch.map(|branch| {
        branch
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    let updated = state
        .projects
        .update(
            access.project.id,
            &access.project.name,
            UpdateProject {
                scm_url,
                default_branch,
            },
        )
        .await?;
    Ok(Json(updated.into()))
}

/// 删除项目：全局管理员发起；所有构建均终态后，原子冻结项目授权并进入
/// 异步删除。项目行在清理完成前保留归属，完成后保留不可访问墓碑。
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{name}",
    tag = "projects",
    responses(
        (status = 202, description = "项目已冻结并进入异步删除", body = crate::store::deletions::DeletionJob),
        (status = 403, description = "仅全局管理员可删除项目", body = ErrorBody),
        (status = 404, description = "项目不存在", body = ErrorBody),
        (status = 409, description = "仍有排队或运行中的构建", body = ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<crate::store::deletions::DeletionJob>), ApiError> {
    let project = state
        .projects
        .get_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("项目 {name} 不存在")))?;
    let job = state
        .deletions
        .enqueue_project(project.id, &auth.username)
        .await
        .map_err(|error| match error {
            crate::store::StoreError::Conflict(message) => ApiError::conflict(message),
            crate::store::StoreError::NotFound(_) => {
                ApiError::resource_not_found(format!("项目 {name} 不存在"))
            }
            other => ApiError::internal("项目删除入队", &other),
        })?;
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            crate::store::audit::AuditEvent::ProjectDeletionRequested,
            Some(&name),
            Some(&serde_json::json!({ "deletion_id": job.id }).to_string()),
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(job)))
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
    if req.scm_type != ScmTypeDto::None && req.scm_url.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "scm_url".into(),
            message: "仓库 URL 不能为空".into(),
        });
    }
    if matches!(req.scm_type, ScmTypeDto::Svn | ScmTypeDto::None) && req.default_branch.is_some() {
        issues.push(ValidationIssue {
            path: "default_branch".into(),
            message: "该项目类型无分支概念，不支持默认分支".into(),
        });
    }
    issues
}

/// 编辑项目的字段校验，与前端编辑契约保持一致。
fn validate_update(project: &Project, req: &UpdateProjectRequest) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if let Some(url) = req.scm_url.as_deref() {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            issues.push(ValidationIssue {
                path: "scm_url".into(),
                message: "仓库 URL 不能为空".into(),
            });
        } else if !(trimmed.starts_with("http://") || trimmed.starts_with("https://"))
            || trimmed.len() <= trimmed.find("://").map_or(0, |index| index + 3)
        {
            issues.push(ValidationIssue {
                path: "scm_url".into(),
                message: "仓库 URL 需以 http:// 或 https:// 开头".into(),
            });
        }
    }
    if matches!(project.scm_type, ScmType::Svn | ScmType::None)
        && req.default_branch.as_ref().is_some_and(Option::is_some)
    {
        issues.push(ValidationIssue {
            path: "default_branch".into(),
            message: "该项目类型无分支概念，不支持默认分支".into(),
        });
    }
    issues
}

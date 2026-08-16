//! 用户管理端点（票 B2b-T4，ADR-0014；目录端点为票 B2b-T5）。
//!
//! 两类面：
//!
//! - **用户管理（全局 admin 专属，[`super::policy::RequireGlobalAdmin`]
//!   声明）**：建号、全量列表、禁用/启用、代办重置密码。禁用在 store 层
//!   同事务级联删除该用户全部 session 与 PAT——旧 cookie 与旧令牌下一请求
//!   即 401；只禁用不物理删除，历史操作人字段永久保留。自注册开关
//!   （register）在 [`super::auth`]。
//! - **用户目录（项目 admin 档）**：最小只读清单（仅 id + 用户名，供成员
//!   分配下拉）——守卫不在项目域 extractor 里（本端点无项目路径参数）：
//!   全局 admin 直接放行，普通用户以「任意项目的 admin」判定（viewer /
//!   runner / 无角色 403）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::auth::{validate_new_account, validate_new_password};
use super::error::{ApiError, ErrorBody, parse_body};
use super::policy::RequireGlobalAdmin;
use crate::auth::hash_password;
use crate::store::users::User;

/// 用户管理视图（密码哈希永不出现；含 disabled——禁用行仍可见可管理）。
#[derive(Debug, Serialize, ToSchema)]
pub struct UserResponse {
    /// 用户 id。
    pub id: i64,
    /// 用户名（唯一）。
    pub username: String,
    /// 全局管理员。
    pub is_admin: bool,
    /// 禁用标志（禁用即踢线：session 与 PAT 同秒全删）。
    pub disabled: bool,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

impl From<User> for UserResponse {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            username: user.username,
            is_admin: user.is_admin,
            disabled: user.disabled,
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }
}

/// 建号请求体（全局 admin）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUserRequest {
    /// 用户名（1-64 位字母数字或 `_ . -`，trim 后生效）。
    pub username: String,
    /// 密码（最小长度 8，无复杂度规则）。
    pub password: String,
    /// 是否全局管理员（默认 false——建号默认普通用户，admin 是显式选择）。
    #[serde(default)]
    pub is_admin: Option<bool>,
}

/// 禁用/启用请求体（全局 admin）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchUserRequest {
    /// 目标状态：true = 禁用（同秒删其全部 session 与 PAT），false = 启用。
    pub disabled: bool,
}

/// 代办重置密码请求体（全局 admin，无需当前密码——ADR-0014：v1 无邮件
/// 自助重置，管理员代办即找回通道）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetPasswordRequest {
    /// 新密码（最小长度 8，无复杂度规则）。
    pub new_password: String,
}

/// 全量用户列表（全局 admin；按用户名排序，含已禁用，无密码哈希）。
#[utoipa::path(
    get,
    path = "/api/v1/users",
    tag = "users",
    responses(
        (status = 200, description = "全部用户（含已禁用，按用户名排序）", body = [UserResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<Vec<UserResponse>>, ApiError> {
    let users = state.users.list().await?;
    Ok(Json(users.into_iter().map(Into::into).collect()))
}

/// 建号（全局 admin；注册开关无关——内网默认建号通道）。
#[utoipa::path(
    post,
    path = "/api/v1/users",
    tag = "users",
    request_body = CreateUserRequest,
    responses(
        (status = 201, description = "已创建", body = UserResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 409, description = "用户名已存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（用户名非空/字符集、密码最小 8 位）", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    body: Bytes,
) -> Result<(StatusCode, Json<UserResponse>), ApiError> {
    let req: CreateUserRequest = parse_body(&body)?;
    validate_new_account(&req.username, &req.password)?;

    let hash = hash_password(&req.password).await;
    let user = state
        .users
        .create(
            req.username.trim(),
            &hash,
            req.is_admin.unwrap_or(false),
        )
        .await?;
    // 审计（票 B2b-T7，ADR-0015）：用户建立——actor 为认证操作人实名，
    // detail 记目标用户名（历史字段永不悬空）。
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            crate::store::audit::AuditEvent::UserCreated,
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(user.into())))
}

/// 禁用 / 启用（全局 admin）。禁用同事务级联删除该用户全部 session 与
/// PAT——旧 cookie 与旧令牌下一请求即 401；用户行永不删除，历史操作人
/// 字段不动。启用后以原密码重新登录。
#[utoipa::path(
    patch,
    path = "/api/v1/users/{name}",
    tag = "users",
    request_body = PatchUserRequest,
    params(("name" = String, Path, description = "用户名")),
    responses(
        (status = 200, description = "已更新，返回落定后的用户行", body = UserResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 404, description = "用户不存在", body = ErrorBody),
        (status = 422, description = "请求体形态不符（缺 disabled 字段）", body = ErrorBody),
    )
)]
pub async fn patch(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<UserResponse>, ApiError> {
    let req: PatchUserRequest = parse_body(&body)?;
    let user = state
        .users
        .get_by_username(&name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("用户 {name} 不存在")))?;
    let updated = state
        .users
        .set_disabled(user.id, req.disabled)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("用户 {name} 不存在")))?;
    // 审计（票 B2b-T7）：禁用/启用同记（detail 记目标用户名与动作）——
    // 禁用是「即时踢线」的安全动作，启用是撤销动作。
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            if req.disabled {
                crate::store::audit::AuditEvent::UserDisabled
            } else {
                crate::store::audit::AuditEvent::UserEnabled
            },
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
    Ok(Json(updated.into()))
}

/// 代办重置密码（全局 admin）：覆写目标用户密码哈希，旧密码即刻失效。
/// 不清凭据面（既有 session/PAT 各有独立吊销途径）。
#[utoipa::path(
    put,
    path = "/api/v1/users/{name}/password",
    tag = "users",
    request_body = ResetPasswordRequest,
    params(("name" = String, Path, description = "用户名")),
    responses(
        (status = 204, description = "已重置：旧密码失效，新密码可登录"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 404, description = "用户不存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（新密码最小 8 位）", body = ErrorBody),
    )
)]
pub async fn reset_password(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let req: ResetPasswordRequest = parse_body(&body)?;
    validate_new_password("new_password", &req.new_password)?;

    let user = state
        .users
        .get_by_username(&name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("用户 {name} 不存在")))?;
    let hash = hash_password(&req.new_password).await;
    state.users.set_password(user.id, &hash).await?;
    // 审计（票 B2b-T7）：管理员代办重置密码（detail 记目标用户名）。
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            crate::store::audit::AuditEvent::PasswordReset,
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 用户目录项（最小形态：仅 id + 用户名）。
#[derive(Debug, Serialize, ToSchema)]
pub struct DirectoryEntryResponse {
    /// 用户 id。
    pub id: i64,
    /// 用户名。
    pub username: String,
}

/// 最小用户目录（项目 admin 档：全局 admin 或任意项目的 admin；仅活跃
/// 用户，排除已禁用）。
#[utoipa::path(
    get,
    path = "/api/v1/users/directory",
    tag = "users",
    responses(
        (status = 200, description = "用户目录（仅 id + 用户名，按用户名排序；排除已禁用）", body = [DirectoryEntryResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非项目或全局 admin（viewer / runner / 无角色不可读）", body = ErrorBody),
    )
)]
pub async fn directory(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<DirectoryEntryResponse>>, ApiError> {
    if !auth.is_admin && !state.members.is_any_project_admin(auth.user_id).await? {
        return Err(ApiError::forbidden(
            "用户目录仅项目或全局管理员可读（成员分配用）",
        ));
    }
    let entries = state
        .users
        .list_directory()
        .await?
        .into_iter()
        .map(|(id, username)| DirectoryEntryResponse { id, username })
        .collect();
    Ok(Json(entries))
}

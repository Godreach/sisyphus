//! 用户目录端点（票 B2b-T5，ADR-0014）：项目 admin 可读最小用户目录
//! （仅 id + 用户名、只读），供成员分配下拉——不必全局管理员代查。
//!
//! 守卫不在 [`super::policy`] 的项目域 extractor 里（本端点无项目路径参数）：
//! 全局 admin 直接放行，普通用户以「任意项目的 admin」判定（ viewer /
//! runner / 无角色 403）。完整用户管理（建 / 禁 / 改密）为全局 admin 专属，
//! 随其批次落地。

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::{ApiError, ErrorBody};

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

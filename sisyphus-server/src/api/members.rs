//! 项目成员端点（票 B2b-T5，ADR-0014）：查看 / 整组分配三档角色。
//!
//! 项目 admin 档（[`super::policy::RequireAdmin`] 声明）。PUT 为整组替换
//! 语义：提交清单即项目成员的完整状态（未列入的成员被移除），角色变更
//! 即时生效（授权逐请求查库，无缓存层）。全局 admin 无成员行也可管理
//! （隐含项目 admin，ADR-0014）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireAdmin;
use crate::auth::Role;
use crate::store::members::MemberRow;

/// 角色的 API 形态（`viewer` / `runner` / `admin`；域类型 [`Role`] 落库文本
/// 同形，转换零成本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoleDto {
    /// 只读（看项目 / 定义 / 构建）。
    Viewer,
    /// viewer + 触发 / 取消 / 重跑（端点随 engine 批次）。
    Runner,
    /// runner + 改定义 / 管成员 / 机密 / 项目设置。
    Admin,
}

impl From<RoleDto> for Role {
    fn from(dto: RoleDto) -> Self {
        match dto {
            RoleDto::Viewer => Self::Viewer,
            RoleDto::Runner => Self::Runner,
            RoleDto::Admin => Self::Admin,
        }
    }
}

impl From<Role> for RoleDto {
    fn from(role: Role) -> Self {
        match role {
            Role::Viewer => Self::Viewer,
            Role::Runner => Self::Runner,
            Role::Admin => Self::Admin,
        }
    }
}

/// PUT 成员清单的条目：用户名 + 角色。
#[derive(Debug, Deserialize, ToSchema)]
pub struct MemberAssignment {
    /// 用户名（须已存在；目录端点 `GET /users/directory` 供下拉）。
    pub username: String,
    /// 分配的角色。
    pub role: RoleDto,
}

/// 成员清单项（GET 响应 / PUT 后回读共用）。
#[derive(Debug, Serialize, ToSchema)]
pub struct MemberResponse {
    /// 用户 id。
    pub user_id: i64,
    /// 用户名。
    pub username: String,
    /// 项目角色。
    pub role: RoleDto,
}

impl From<MemberRow> for MemberResponse {
    fn from(row: MemberRow) -> Self {
        Self {
            user_id: row.user_id,
            username: row.username,
            role: row.role.into(),
        }
    }
}

/// 查看项目成员（项目 admin 档；全局 admin 的隐含成员关系不在此列，
/// ADR-0014：避免噪声，仍可显式分配以显式化）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/members",
    tag = "projects",
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 200, description = "成员清单（按用户名排序）", body = [MemberResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（成员管理需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
) -> Result<Json<Vec<MemberResponse>>, ApiError> {
    let rows = state.members.list_by_project(access.project.id).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// 整组分配项目成员（PUT 语义：提交清单即完整状态，未列入者移除；
/// 角色变更即时生效）。
#[utoipa::path(
    put,
    path = "/api/v1/projects/{name}/members",
    tag = "projects",
    request_body = [MemberAssignment],
    params(("name" = String, Path, description = "项目名")),
    responses(
        (status = 200, description = "已分配，返回落定后的成员清单", body = [MemberResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "项目权限不足（成员管理需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在或不可见", body = ErrorBody),
        (status = 422, description = "输入校验失败（用户名空白 / 重复 / 不存在）", body = ErrorBody),
    )
)]
pub async fn replace(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    body: Bytes,
) -> Result<Json<Vec<MemberResponse>>, ApiError> {
    let assignments: Vec<MemberAssignment> = parse_body(&body)?;

    // 先整组校验（用户名非空、不重复），再整组解析用户名 → id：任一用户
    // 不存在即 422 且不落任何变更（replace_all 的事务只兜底部分失败）。
    let mut issues = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, assignment) in assignments.iter().enumerate() {
        let username = assignment.username.trim();
        if username.is_empty() {
            issues.push(ValidationIssue {
                path: format!("members[{i}].username"),
                message: "用户名不能为空".into(),
            });
        } else if !seen.insert(username) {
            issues.push(ValidationIssue {
                path: format!("members[{i}].username"),
                message: format!("成员清单中用户名重复：{username}"),
            });
        }
    }
    let mut resolved = Vec::with_capacity(assignments.len());
    for (i, assignment) in assignments.iter().enumerate() {
        let username = assignment.username.trim();
        if username.is_empty() {
            continue; // 空名已在上面记过 issue
        }
        match state.users.get_by_username(username).await? {
            Some(user) => resolved.push((user.id, assignment.role.into())),
            None => issues.push(ValidationIssue {
                path: format!("members[{i}].username"),
                message: format!("用户不存在：{username}"),
            }),
        }
    }
    if !issues.is_empty() {
        return Err(ApiError::validation("成员清单校验失败", issues));
    }

    state
        .members
        .replace_all(access.project.id, &resolved)
        .await?;
    let rows = state.members.list_by_project(access.project.id).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

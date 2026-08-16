//! 授权 extractor（票 B2b-T5/T4，ADR-0014）：项目域端点只声明档位、全局
//! 资源端点声明全局 admin——不实现判定。权限矩阵本体在 [`crate::auth`]
//! （policy 单点集中），本模块只做「从路径解析项目 → 查成员角色 → 全局
//! admin 视作项目 admin → 判档位」的通用装配。
//!
//! 判定序列与错误形态（矩阵之外仅有的策略语义）：
//!
//! 1. 项目不存在 → 404；
//! 2. 认证用户无该项目的成员角色 → **404**（与不存在同形：已登录用户不可
//!    借 403/404 之辨探测项目存在性，ADR-0014 用户故事 25）；
//! 3. 有角色但档位不足（矩阵判定失败）→ 403；
//! 4. 全局 admin → 视作项目 admin（无需成员行，ADR-0014 隐含权限）。
//!
//! 认证（401）由 `/api/v1` 全局中间件先行把关，本模块只消费其注入的
//! [`AuthContext`]。漏声明 extractor 的端点在 401 后全放行——以 OpenAPI
//! snapshot + code review 兜底（ADR-0014，v1 不做静态扫描）。

use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::extract::Path;
use axum::http::request::Parts;

use super::AppState;
use super::auth::AuthContext;
use super::error::ApiError;
use crate::auth::{Permission, Role};
use crate::store::projects::Project;

/// 授权通过的产物：已解析的项目行（handler 免二次查询）+ 调用者在该项目的
/// 有效角色（全局 admin 无成员行也是 [`Role::Admin`]）+ 操作人实名
/// （认证用户名的「操作人」语义，票 B2b-T5；机密 updated_by / 审计 actor
/// 消费）。
#[derive(Debug, Clone)]
pub struct ProjectAccess {
    /// 项目行（extractor 已查库裁决存在性与可见性）。
    pub project: Project,
    /// 有效角色（显式成员角色，或全局 admin 的隐含 admin）。
    pub role: Role,
    /// 操作人实名（认证用户名；历史字段永不悬空，与 pipeline operator
    /// 同纪律）。
    pub operator: String,
}

/// 声明 viewer 档位（[`Permission::View`]）：查看项目 / 定义 / 构建。
/// handler 解构出 [`ProjectAccess`] 直接用。
pub struct RequireViewer(pub ProjectAccess);

/// 声明项目 admin 档位（[`Permission::Manage`]）：定义保存、成员管理、
/// 机密管理、项目设置。
pub struct RequireAdmin(pub ProjectAccess);

/// 声明全局 admin 档（票 B2b-T4，ADR-0014）：全局资源专属动作（用户管理、
/// 建号、代办重置密码）的端点声明。认证中间件先行把关（401），这里只判
/// [`AuthContext::is_admin`]：不足 403——用户管理面对全部已登录用户可见
/// 存在，无「不可见」语义（与项目域的 404 双源同形不同题）。
pub struct RequireGlobalAdmin(pub AuthContext);

impl FromRequestParts<AppState> for RequireGlobalAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let axum::Extension(auth) = axum::Extension::<AuthContext>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::unauthorized())?;
        if !auth.is_admin {
            return Err(ApiError::forbidden("该操作仅全局管理员可用"));
        }
        Ok(RequireGlobalAdmin(auth))
    }
}

impl FromRequestParts<AppState> for RequireViewer {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        resolve(parts, state, Permission::View)
            .await
            .map(RequireViewer)
    }
}

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        resolve(parts, state, Permission::Manage)
            .await
            .map(RequireAdmin)
    }
}

/// 通用裁决：路径 `{name}` 解析项目 → 角色解析 → 矩阵判定。两个 extractor
/// 只差声明的 [`Permission`]，矩阵本体不在本模块。
async fn resolve(
    parts: &mut Parts,
    state: &AppState,
    permission: Permission,
) -> Result<ProjectAccess, ApiError> {
    // 认证中间件（外层）先行注入；理论缺失按未认证对待（路由结构错配）。
    let axum::Extension(auth) = axum::Extension::<AuthContext>::from_request_parts(parts, state)
        .await
        .map_err(|_| ApiError::unauthorized())?;

    // 项目名取自路径参数 `name`（项目域路由的首段参数，形态由路由结构
    // 保证；非消费式读取，handler 仍可再取其余路径参数）。
    let Path(params) = Path::<HashMap<String, String>>::from_request_parts(parts, state)
        .await
        .map_err(|e| ApiError::internal("path params", &e))?;
    let name = params
        .get("name")
        .ok_or_else(|| ApiError::internal("path params", &"路由缺 {name} 参数"))?;

    // 404 双源同形：项目不存在，或存在但对调用者不可见（无角色）。
    let not_visible = || ApiError::resource_not_found(format!("项目 {name} 不存在"));
    let project = state
        .projects
        .get_by_name(name)
        .await?
        .ok_or_else(not_visible)?;
    let role = if auth.is_admin {
        Role::Admin
    } else {
        state
            .members
            .role_of(project.id, auth.user_id)
            .await?
            .ok_or_else(not_visible)?
    };
    if !role.satisfies(permission) {
        return Err(ApiError::forbidden(format!(
            "项目 {name} 权限不足：{} 需要 {} 及以上档位",
            permission_label(permission),
            permission.min_role().as_str()
        )));
    }
    Ok(ProjectAccess {
        project,
        role,
        operator: auth.username,
    })
}

/// 动作的人读名（403 message 用；与矩阵同处演进）。
fn permission_label(permission: Permission) -> &'static str {
    match permission {
        Permission::View => "查看项目",
        Permission::Run => "触发/取消构建",
        Permission::Manage => "项目管理操作",
    }
}

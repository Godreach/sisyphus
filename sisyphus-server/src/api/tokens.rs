//! PAT 管理端点（票 B2b-T3，ADR-0014）：创建（响应一次性返回完整令牌）/
//! 列表（只有名/时间/过期）/ 吊销（删行，立即失效）。
//!
//! 权限 = owner 本人（v1 无 scope 细分）：全部端点只操作当前认证用户的
//! 行（[`AuthContext::user_id`]），吊销以 id + 属主双条件命中，他人的 id
//! 一律 404（不暴露存在性）。令牌明文只在创建响应出现一次，此后任何
//! 端点不再回显——列表与吊销响应均无值形态。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use crate::auth::{TokenFamily, generate_token, token_hash};
use crate::store::StoreError;
use crate::store::now_ms;
use crate::store::tokens::PatRow;

/// token 哈希撞唯一约束的换 token 重试次数（32 字节随机碰撞，概率上不可达）。
const TOKEN_ATTEMPTS: usize = 3;

/// 创建 PAT 请求体。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTokenRequest {
    /// 令牌名（管理用，如 `ci-deploy`；非空，trim 后生效）。
    pub name: String,
    /// 过期时间（Unix 毫秒；`null` = 永不过期）。须晚于当前时间。
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// PAT 列表项（无值形态：名 / 创建时间 / 过期——令牌值任何端点不再出现）。
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenResponse {
    /// 行 id（吊销端点的路径参数）。
    pub id: i64,
    /// 令牌名。
    pub name: String,
    /// 过期时间（Unix 毫秒；`null` = 永不过期）。
    pub expires_at: Option<i64>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
}

impl From<PatRow> for TokenResponse {
    fn from(row: PatRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            expires_at: row.expires_at,
            created_at: row.created_at,
        }
    }
}

/// 创建 PAT 响应：完整令牌仅此一次返回（此后任何端点不再出现值）。
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedTokenResponse {
    /// 完整令牌（`sis_` 前缀 + 43 字符）。请立即保存：本响应是唯一一次
    /// 出现，库里只存其 SHA-256，任何端点都无法找回。
    pub token: String,
    /// 行 id。
    pub id: i64,
    /// 令牌名。
    pub name: String,
    /// 过期时间（Unix 毫秒；`null` = 永不过期）。
    pub expires_at: Option<i64>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
}

/// 创建 PAT（需认证，权限 = owner 本人）。
#[utoipa::path(
    post,
    path = "/api/v1/auth/tokens",
    tag = "auth",
    request_body = CreateTokenRequest,
    responses(
        (status = 201, description = "已创建；token 值仅此一次返回，此后任何端点不再出现", body = CreatedTokenResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 422, description = "输入校验失败（名非空、过期时间须晚于当前时间）", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Bytes,
) -> Result<(StatusCode, Json<CreatedTokenResponse>), ApiError> {
    let req: CreateTokenRequest = parse_body(&body)?;
    let now = now_ms();
    validate_create(&req, now)?;

    // 生成 → 哈希落库；撞唯一约束（概率上不可达）换 token 重试。
    for _ in 0..TOKEN_ATTEMPTS {
        let token = generate_token(TokenFamily::Pat);
        match state
            .pats
            .insert(
                auth.user_id,
                req.name.trim(),
                &token_hash(&token),
                req.expires_at,
                now,
            )
            .await
        {
            Ok(row) => {
                // 审计（票 B2b-T7，ADR-0015）：PAT 建立——detail 记令牌名
                // （永不记值：值只在本响应出现一次）。
                state
                    .audit
                    .insert(
                        now,
                        &auth.username,
                        crate::store::audit::AuditEvent::PatCreated,
                        None,
                        Some(&serde_json::json!({ "name": row.name }).to_string()),
                    )
                    .await?;
                return Ok((
                    StatusCode::CREATED,
                    Json(CreatedTokenResponse {
                        token,
                        id: row.id,
                        name: row.name,
                        expires_at: row.expires_at,
                        created_at: row.created_at,
                    }),
                ));
            }
            Err(StoreError::Unique(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::internal(
        "token create",
        &"连续撞唯一约束，随机源疑似异常",
    ))
}

/// 列出当前用户全部 PAT（按创建时间升序；无值形态）。
#[utoipa::path(
    get,
    path = "/api/v1/auth/tokens",
    tag = "auth",
    responses(
        (status = 200, description = "当前用户的 PAT 清单（名 / 创建时间 / 过期，永不含令牌值）", body = [TokenResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<TokenResponse>>, ApiError> {
    let rows = state.pats.list_by_user(auth.user_id).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// 吊销 PAT（删行，下一请求即 401）。
#[utoipa::path(
    delete,
    path = "/api/v1/auth/tokens/{id}",
    tag = "auth",
    params(("id" = i64, Path, description = "PAT 行 id（列表返回的 id）")),
    responses(
        (status = 204, description = "已吊销：行已删，该令牌即刻失效"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "id 不存在或不属于当前用户（不暴露存在性）", body = ErrorBody),
    )
)]
pub async fn revoke(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // 路径参数按字符串取再解析：非数字 id 与「不存在」同形（统一 JSON
    // 404，不落 axum 默认的纯文本拒绝）。
    let id = id
        .parse::<i64>()
        .map_err(|_| ApiError::resource_not_found("令牌不存在"))?;
    // 吊销前取行（令牌名落审计 detail）：先查后删——他人 id 在查的阶段
    // 即 404（不暴露存在性，与删后 404 同形）。
    let pat = state
        .pats
        .get_by_user(auth.user_id, id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found("令牌不存在"))?;
    state.pats.delete(auth.user_id, id).await?;
    // 审计（票 B2b-T7）：PAT 吊销——detail 记令牌名（值永不落审计）。
    state
        .audit
        .insert(
            crate::store::now_ms(),
            &auth.username,
            crate::store::audit::AuditEvent::PatRevoked,
            None,
            Some(&serde_json::json!({ "name": pat.name }).to_string()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 创建输入校验：名非空；过期时间（若有）须晚于当前时间（生出即死的
/// 令牌只会制造困惑，建出来也过不了认证）。时间由调用侧传入，与写入
/// 落库的 created_at 同一取值。
fn validate_create(req: &CreateTokenRequest, now: i64) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if req.name.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "name".into(),
            message: "令牌名不能为空".into(),
        });
    }
    if req.expires_at.is_some_and(|expires_at| expires_at <= now) {
        issues.push(ValidationIssue {
            path: "expires_at".into(),
            message: "过期时间必须晚于当前时间".into(),
        });
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("令牌输入校验失败", issues))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str, expires_at: Option<i64>) -> CreateTokenRequest {
        CreateTokenRequest {
            name: name.into(),
            expires_at,
        }
    }

    #[test]
    fn validate_create_enforces_name_and_future_expiry() {
        let now = now_ms();
        assert!(validate_create(&req("ci", None), now).is_ok());
        assert!(validate_create(&req("  ci  ", Some(now + 86_400_000)), now).is_ok());

        // 名空白 / 过去与当下的过期时间：422（错误清单定位路径由 Router
        // 缝断言，见 tests/pat_auth.rs）。
        for (name, expires_at) in [("   ", None), ("ci", Some(0)), ("ci", Some(now))] {
            let err = validate_create(&req(name, expires_at), now).unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name}/{expires_at:?} 应 422"
            );
        }
    }
}

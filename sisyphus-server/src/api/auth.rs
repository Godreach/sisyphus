//! 认证端点与中间件（票 B2b-T1/T2，ADR-0014）：setup wizard / login /
//! logout / me、挂在 `/api/v1` 受保护段全局面的会话认证中间件，以及
//! login 上的进程内限流（per-IP + per-username）。
//!
//! - 认证（401）是全局 middleware：cookie 里的 session id → SHA-256 查行 →
//!   未过期 → 用户未禁用，通过即顺延 7 天（滑动过期）并把
//!   [`AuthContext`] 注入请求扩展；失败/缺失一律 401 统一 JSON 形态。
//! - 放行面即路由结构（[`super::router`]）：login 与 setup 挂公开段不经此
//!   中间件（healthz 与静态资源面不在 `/api/v1` 下，天然不拦）；setup 的
//!   「空库限定」由 handler 裁决——用户表非空一律 404，不暴露实例状态。
//! - cookie：HttpOnly + SameSite=Lax + Path=/，v1 不设 Secure（跨公网走 TLS
//!   的立场随部署文档，ADR-0014）。CSRF 面在 [`super::csrf`]（B2b-T2）；
//!   授权（403 extractor）与 PAT 随后续批次。

use axum::Json;
use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde::Serialize;
use std::net::SocketAddr;
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use crate::auth::{
    MIN_PASSWORD_LEN, SESSION_COOKIE_NAME, SESSION_MAX_AGE_SECS, SESSION_TTL_MS,
    generate_session_id, hash_password, session_id_hash, verify_password,
};
use crate::store::StoreError;
use crate::store::now_ms;

/// session id 撞主键的换号重试次数（32 字节随机碰撞，概率上不可达）。
const SESSION_ID_ATTEMPTS: usize = 3;

/// 认证通过的请求上下文（中间件注入请求扩展，handler 经 `Extension` 取）。
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// 用户 id。
    pub user_id: i64,
    /// 用户名。
    pub username: String,
    /// 全局管理员。
    pub is_admin: bool,
    /// 本会话的 id 哈希（登出删行用）。
    pub session_id_hash: String,
}

/// setup wizard / login 共用的凭据请求体。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CredentialsRequest {
    /// 用户名。
    pub username: String,
    /// 密码（最小长度 8，无复杂度规则）。
    pub password: String,
}

/// 当前用户视图（setup / login / me 共用；SPA 引导用）。
#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    /// 用户名。
    pub username: String,
    /// 是否全局管理员。
    pub is_admin: bool,
}

/// setup wizard：空库时创建首个全局管理员。
#[utoipa::path(
    post,
    path = "/api/v1/auth/setup",
    tag = "auth",
    request_body = CredentialsRequest,
    responses(
        (status = 201, description = "首个全局管理员已创建", body = MeResponse),
        (status = 404, description = "用户表非空：端点不可用（不暴露状态）", body = ErrorBody),
        (status = 422, description = "输入校验失败（用户名非空、密码最小 8 位）", body = ErrorBody),
    )
)]
pub async fn setup(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<MeResponse>), ApiError> {
    // 空库判定先行：非空库对任何输入（含非法输入）一律 404，不借校验错误
    // 泄露「端点还活着」。
    if state.users.count().await? != 0 {
        return Err(ApiError::not_found());
    }
    let req: CredentialsRequest = parse_body(&body)?;
    validate_credentials(&req)?;

    let hash = hash_password(&req.password).await;
    let user = state.users.create(req.username.trim(), &hash, true).await?;
    Ok((
        StatusCode::CREATED,
        Json(MeResponse {
            username: user.username,
            is_admin: user.is_admin,
        }),
    ))
}

/// 登录：用户名密码换取会话 cookie。
///
/// 登录限流（票 B2b-T2，ADR-0014）：per-IP（直连地址，v1 不信任
/// X-Forwarded-For，反代场景写进部署文档）与 per-username 双键独立计数，
/// 任一键冷却中即 429；失败对双键各记一次，成功对双键清零。
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = CredentialsRequest,
    responses(
        (status = 200, description = "登录成功，Set-Cookie 下发会话（HttpOnly + SameSite=Lax + Path=/）", body = MeResponse),
        (status = 401, description = "用户名或密码错误（不区分两者）", body = ErrorBody),
        (status = 422, description = "请求体不是合法 JSON 或形态不符", body = ErrorBody),
        (status = 429, description = "登录尝试过于频繁（per-IP / per-username 连续 5 败进入冷却，随连续触发递增、封顶 15 分钟）", body = ErrorBody),
    )
)]
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: CredentialsRequest = parse_body(&body)?;
    let ip = addr.ip().to_string();
    let username = req.username.trim().to_string();

    // 限流双键：直连 IP 与用户名（用户名 trim 后与查找同形）。任一冷却
    // 中即 429——正确密码也不放行（暴破拖慢是本面的唯一目标）。
    let now = now_ms();
    if let Some(retry_after_ms) = state.login_limiter.check_login(&ip, &username, now) {
        return Ok(rate_limited(retry_after_ms));
    }

    let user = match state.users.get_by_username(&username).await? {
        Some(user) => user,
        None => {
            // 防用户枚举的时间侧信道：用户不存在时也跑一次真哈希校验，
            // 与「用户在、密码错」分支耗时一致（响应形态本就一致）。
            verify_password(&req.password, &dummy_hash().await).await;
            state
                .login_limiter
                .record_login_failure(&ip, &username, now);
            return Err(ApiError::unauthorized());
        }
    };
    if user.disabled || !verify_password(&req.password, &user.password_hash).await {
        state
            .login_limiter
            .record_login_failure(&ip, &username, now);
        return Err(ApiError::unauthorized());
    }
    state.login_limiter.record_login_success(&ip, &username);

    // 建会话：撞主键（概率上不可达）换 id 重试。
    let mut session_id = None;
    for _ in 0..SESSION_ID_ATTEMPTS {
        let id = generate_session_id();
        match state
            .sessions
            .insert(&session_id_hash(&id), user.id, now, now + SESSION_TTL_MS)
            .await
        {
            Ok(()) => {
                session_id = Some(id);
                break;
            }
            Err(StoreError::Unique(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let Some(session_id) = session_id else {
        return Err(ApiError::internal(
            "session create",
            &"连续撞主键，随机源疑似异常",
        ));
    };

    let me = MeResponse {
        username: user.username,
        is_admin: user.is_admin,
    };
    let mut resp = (StatusCode::OK, Json(me)).into_response();
    set_session_cookie(&mut resp, &session_id);
    Ok(resp)
}

/// 登出：删会话行（原 cookie 即刻失效）并清 cookie。
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "已登出：会话删除 + Set-Cookie 清空（需认证）"),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn logout(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Response, ApiError> {
    state.sessions.delete(&auth.session_id_hash).await?;
    let mut resp = StatusCode::NO_CONTENT.into_response();
    clear_session_cookie(&mut resp);
    Ok(resp)
}

/// 当前用户（SPA 引导用；需认证）。
#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "当前用户名与全局管理员标志", body = MeResponse),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn me(axum::Extension(auth): axum::Extension<AuthContext>) -> Json<MeResponse> {
    Json(MeResponse {
        username: auth.username,
        is_admin: auth.is_admin,
    })
}

/// 会话认证中间件（挂在 `/api/v1` 受保护段全局面）：cookie → session 行 →
/// 用户，通过注入 [`AuthContext`] 并顺延过期；失败/缺失 401 统一形态。
/// 放行面由路由结构决定（login/setup 在公开段，不经过本中间件）。
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(session_id) = cookie_value(req.headers(), SESSION_COOKIE_NAME) else {
        return ApiError::unauthorized().into_response();
    };
    let hash = session_id_hash(&session_id);
    let now = now_ms();

    // session 行（未过期）→ 用户（未禁用）→ 注入上下文 + 滑动顺延。
    let session = match state.sessions.get_valid(&hash, now).await {
        Ok(Some(session)) => session,
        Ok(None) => return ApiError::unauthorized().into_response(),
        Err(e) => return ApiError::internal("session lookup", &e).into_response(),
    };
    let user = match state.users.get_by_id(session.user_id).await {
        Ok(Some(user)) if !user.disabled => user,
        Ok(_) => return ApiError::unauthorized().into_response(),
        Err(e) => return ApiError::internal("user lookup", &e).into_response(),
    };
    if let Err(e) = state.sessions.touch(&hash, now + SESSION_TTL_MS).await {
        return ApiError::internal("session touch", &e).into_response();
    }

    req.extensions_mut().insert(AuthContext {
        user_id: user.id,
        username: user.username,
        is_admin: user.is_admin,
        session_id_hash: hash,
    });
    let resp = next.run(req).await;
    // 滑动过期在浏览器侧收口：随认证响应续发同值 cookie（刷新 Max-Age），
    // 否则浏览器在登录后第 7 天整丢弃 cookie，服务端顺延不再可见。
    // handler 已带 Set-Cookie 时（logout 的清空头）不覆盖。
    if !resp.headers().contains_key(header::SET_COOKIE) {
        let mut resp = resp;
        set_session_cookie(&mut resp, &session_id);
        return resp;
    }
    resp
}

/// setup/login 的输入校验（密码最小长度 8，无复杂度规则，ADR-0014）。
fn validate_credentials(req: &CredentialsRequest) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if req.username.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "username".into(),
            message: "用户名不能为空".into(),
        });
    }
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        issues.push(ValidationIssue {
            path: "password".into(),
            message: format!("密码最小长度 {MIN_PASSWORD_LEN}（无复杂度规则）"),
        });
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("凭据输入校验失败", issues))
    }
}

/// 从 Cookie 头取指定名的值（值域为 base64url，`split_once('=')` 即安全；
/// csrf 中间件共用）。
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
}

/// 登录响应下发会话 cookie（HttpOnly + SameSite=Lax + Path=/；v1 不设
/// Secure；Max-Age 与服务端 7 天滑动过期对齐——浏览器关了再开仍在登录态）。
fn set_session_cookie(resp: &mut Response, session_id: &str) {
    let cookie = format!(
        "{SESSION_COOKIE_NAME}={session_id}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_MAX_AGE_SECS}"
    );
    insert_set_cookie(resp, &cookie);
}

/// 登出响应清空会话 cookie。
fn clear_session_cookie(resp: &mut Response) {
    let cookie = format!("{SESSION_COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    insert_set_cookie(resp, &cookie);
}

/// 登录限流 429：统一错误形态（detail 携带剩余毫秒）+ 标准 `Retry-After`
/// 头（秒，向上取整、至少 1）。
fn rate_limited(retry_after_ms: i64) -> Response {
    let mut resp = ApiError::too_many_requests(retry_after_ms).into_response();
    let secs = ((retry_after_ms + 999) / 1000).max(1).to_string();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, secs.parse().expect("秒数为合法头值"));
    resp
}

fn insert_set_cookie(resp: &mut Response, cookie: &str) {
    let value =
        axum::http::HeaderValue::from_str(cookie).expect("cookie 只含 base64url 与 ASCII 属性");
    resp.headers_mut().insert(header::SET_COOKIE, value);
}

/// 用户枚举诱饵哈希（惰性生成一次，经 spawn_blocking 不占执行器线程；
/// 与真实验证同参数，等耗时）。
async fn dummy_hash() -> String {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(hash) = DUMMY.get() {
        return hash.clone();
    }
    let hash = hash_password("dummy-password").await;
    let _ = DUMMY.set(hash.clone());
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_value_parses_target_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=1; sisyphus_session=abc123_-; last=x"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE_NAME).as_deref(),
            Some("abc123_-")
        );
        assert_eq!(cookie_value(&headers, "absent"), None);

        let headers = HeaderMap::new();
        assert_eq!(cookie_value(&headers, SESSION_COOKIE_NAME), None);
    }

    #[test]
    fn validate_credentials_enforces_min_length_only() {
        let creds = |u: &str, p: &str| CredentialsRequest {
            username: u.into(),
            password: p.into(),
        };
        assert!(validate_credentials(&creds("root", "12345678")).is_ok());
        // 长口令无复杂度要求：纯数字也过。
        assert!(validate_credentials(&creds("root", "12345678901234567890")).is_ok());

        let err = validate_credentials(&creds("  ", "short")).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

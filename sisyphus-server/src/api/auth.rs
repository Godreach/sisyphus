//! 认证端点与中间件（票 B2b-T1/T2/T3/T4，ADR-0014）：setup wizard /
//! register / login / logout / me / 自助改密，挂在 `/api/v1` 受保护段全局面
//! 的认证中间件（cookie 会话 + Bearer PAT 双通道，T3），以及 login 上的
//! 进程内限流（per-IP + per-username）。
//!
//! - 认证（401）是全局 middleware，两通道同权重放：cookie 里的 session id
//!   → SHA-256 查行 → 未过期 → 用户未禁用，通过即顺延 7 天（滑动过期）；
//!   或 `Authorization: Bearer sis_…`（PAT，T3）→ 哈希查行 → 未过期 →
//!   用户未禁用。携 Bearer scheme 的 Authorization 头按显式凭据对待
//!   （优先于 cookie，与 CSRF 中间件的「Bearer 免疫」同一模型）；失败/
//!   缺失一律 401 统一 JSON 形态。通过即把 [`AuthContext`]（含认证通道）
//!   注入请求扩展。
//! - 放行面即路由结构（[`super::router`]）：login、register 与 setup 挂公开
//!   段不经此中间件（healthz 与静态资源面不在 `/api/v1` 下，天然不拦）；
//!   setup 的「空库限定」与 register 的「开关限定」由 handler 裁决。
//! - cookie：HttpOnly + SameSite=Lax + Path=/，v1 不设 Secure（跨公网走 TLS
//!   的立场随部署文档，ADR-0014）。CSRF 面在 [`super::csrf`]（B2b-T2）；
//!   授权（403 extractor）在 [`super::policy`]。PAT 管理端点在
//!   [`super::tokens`]；用户管理（全局 admin）在 [`super::users`]（T4）。

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
    MIN_PASSWORD_LEN, SESSION_COOKIE_NAME, SESSION_MAX_AGE_SECS, SESSION_TTL_MS, TokenFamily,
    generate_session_id, hash_password, session_id_hash, token_family, token_hash, verify_password,
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
    /// 认证通道（会话面动作按通道分岔：登出删行、滑动续发 cookie 只对
    /// cookie 会话有意义；Bearer PAT 无会话行）。
    pub channel: AuthChannel,
}

/// 认证通道（[`AuthContext`] 携带）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthChannel {
    /// cookie 会话：认证动作的落点见 `id_hash` 字段。
    Session {
        /// session id 的 SHA-256（登出删行、认证响应续发同值 cookie 刷新
        /// Max-Age 的落点）。
        id_hash: String,
    },
    /// Bearer PAT：无会话行——登出无事可做（PAT 吊销走 DELETE
    /// /auth/tokens/{id}），响应不续发 cookie。
    Pat,
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
        (status = 422, description = "输入校验失败（用户名非空/字符集、密码最小 8 位）", body = ErrorBody),
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
    validate_new_account(&req.username, &req.password)?;

    let hash = hash_password(&req.password).await;
    let user = state.users.create(req.username.trim(), &hash, true).await?;
    // 审计（票 B2b-T7，ADR-0015）：首个全局 admin 的建立也是安全事件——
    // 回放面从这里起账。detail 记目标用户名（与全局 admin 建号同形态，
    // 审计回放对 user_created 有稳定 schema）。
    state
        .audit
        .insert(
            now_ms(),
            &user.username,
            crate::store::audit::AuditEvent::UserCreated,
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(MeResponse {
            username: user.username,
            is_admin: user.is_admin,
        }),
    ))
}

/// 自注册（票 B2b-T4，ADR-0014）：注册开关（config `[auth]
/// registration_enabled`，默认关）打开时自建**非管理员**账号；关闭时一律
/// 403（内网由全局 admin 建号）。空库时同样 403——首个账号必须经 setup
/// wizard 成为全局 admin，否则自注册出普通用户后 setup 永久 404、实例
/// 再无管理员入口。成功响应不带会话（与 setup 同形：登录是独立一步）。
#[utoipa::path(
    post,
    path = "/api/v1/auth/register",
    tag = "auth",
    request_body = CredentialsRequest,
    responses(
        (status = 201, description = "非管理员账号已创建（不带会话，登录是独立一步）", body = MeResponse),
        (status = 403, description = "注册开关未开启，或用户表为空（先走 setup wizard）", body = ErrorBody),
        (status = 409, description = "用户名已存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（用户名非空/字符集、密码最小 8 位）", body = ErrorBody),
    )
)]
pub async fn register(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<MeResponse>), ApiError> {
    // 开关判定先行（关时对任何输入一律 403，不借校验错误泄露开关状态）。
    if !state.registration_enabled {
        return Err(ApiError::forbidden(
            "注册开关未开启：账号由全局管理员创建",
        ));
    }
    if state.users.count().await? == 0 {
        return Err(ApiError::forbidden(
            "注册不可用：请先完成初始化引导（创建全局管理员）",
        ));
    }
    let req: CredentialsRequest = parse_body(&body)?;
    validate_new_account(&req.username, &req.password)?;

    let hash = hash_password(&req.password).await;
    let user = state.users.create(req.username.trim(), &hash, false).await?;
    // 审计（票 B2b-T7）：自注册建号与全局 admin 建号同为 user_created
    // （detail 同形态：记目标用户名）。
    state
        .audit
        .insert(
            now_ms(),
            &user.username,
            crate::store::audit::AuditEvent::UserCreated,
            None,
            Some(&serde_json::json!({ "username": user.username }).to_string()),
        )
        .await?;
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
        // 审计（票 B2b-T7）：限流触达也是失败认证——暴破被拖慢的轨迹要能
        // 从审计面看到（登录失败事件，actor 为被尝试的用户名）。
        record_login_failure(&state, &username, now).await?;
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
            record_login_failure(&state, &username, now).await?;
            return Err(ApiError::unauthorized());
        }
    };
    if user.disabled || !verify_password(&req.password, &user.password_hash).await {
        state
            .login_limiter
            .record_login_failure(&ip, &username, now);
        record_login_failure(&state, &username, now).await?;
        return Err(ApiError::unauthorized());
    }
    state.login_limiter.record_login_success(&ip, &username);
    record_login_success(&state, &username, now).await?;

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
///
/// Bearer PAT 通道无会话可结束：204 无动作（PAT 的失效面是吊销端点，
/// [`super::tokens::revoke`]）。
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "已登出：会话删除 + Set-Cookie 清空（需认证；Bearer PAT 通道无事可做，同 204）"),
        (status = 401, description = "未认证", body = ErrorBody),
    )
)]
pub async fn logout(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let AuthChannel::Session { id_hash } = &auth.channel {
        state.sessions.delete(id_hash).await?;
        clear_session_cookie(&mut resp);
        // 审计（票 B2b-T7）：只记真正结束了会话的登出（cookie 通道）；
        // Bearer 通道无事可做，不制造空事件。
        state
            .audit
            .insert(
                now_ms(),
                &auth.username,
                crate::store::audit::AuditEvent::Logout,
                None,
                None,
            )
            .await?;
    }
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

/// 自助改密请求体（票 B2b-T4）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    /// 当前密码（核验通过才允许改）。
    pub current_password: String,
    /// 新密码（最小长度 8，无复杂度规则）。
    pub new_password: String,
}

/// 自助改密（票 B2b-T4，ADR-0014）：需验当前密码，验错 403 拒绝——不经
/// 管理员即可轮换自己的凭据。只换哈希：既有会话与 PAT 不受牵连（各自有
/// 独立失效途径：登出 / 吊销），管理员代办重置在 [`super::users::reset_password`]。
#[utoipa::path(
    post,
    path = "/api/v1/auth/password",
    tag = "auth",
    request_body = ChangePasswordRequest,
    responses(
        (status = 204, description = "已改密：新密码下次登录生效，既有会话/PAT 不受牵连"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "当前密码不正确", body = ErrorBody),
        (status = 422, description = "输入校验失败（新密码最小 8 位）", body = ErrorBody),
    )
)]
pub async fn change_password(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let req: ChangePasswordRequest = parse_body(&body)?;
    validate_new_password("new_password", &req.new_password)?;

    // 认证中间件刚核过用户行，这里按 id 再取一次（拿哈希做核验）；行缺失
    // 兜 401 与中间件同形。
    let user = state
        .users
        .get_by_id(auth.user_id)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    if !verify_password(&req.current_password, &user.password_hash).await {
        return Err(ApiError::forbidden("当前密码不正确"));
    }
    let hash = hash_password(&req.new_password).await;
    state.users.set_password(auth.user_id, &hash).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 认证中间件（挂在 `/api/v1` 受保护段全局面）：Bearer PAT 或 cookie
/// 会话 → 用户，通过注入 [`AuthContext`]；失败/缺失 401 统一形态。放行面
/// 由路由结构决定（login/setup 在公开段，不经过本中间件）。
///
/// 携 Bearer scheme 的 Authorization 头按显式凭据对待（优先于 cookie，与
/// CSRF 中间件的「Bearer 免疫」同一模型）；其它 scheme（Basic 等）回落
/// cookie 面。会话通道认证通过即顺延 7 天（滑动过期）并随响应续发同值
/// cookie；PAT 通道无会话动作。
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let now = now_ms();

    // Bearer PAT 面：族边界（两族不混用，Agent 族走自己的认证面）→ 哈希
    // 查行（过期与吊销同表为 None）→ 属主未禁用。
    if let Some(token) = bearer_token(req.headers()) {
        if token_family(&token) != Some(TokenFamily::Pat) {
            return ApiError::unauthorized().into_response();
        }
        let hash = token_hash(&token);
        let pat = match state.pats.find_valid_by_hash(&hash, now).await {
            Ok(Some(pat)) => pat,
            Ok(None) => return ApiError::unauthorized().into_response(),
            Err(e) => return ApiError::internal("token lookup", &e).into_response(),
        };
        let user = match active_user_or_reject(&state, pat.user_id).await {
            Ok(user) => user,
            Err(resp) => return resp,
        };
        req.extensions_mut().insert(AuthContext {
            user_id: user.id,
            username: user.username,
            is_admin: user.is_admin,
            channel: AuthChannel::Pat,
        });
        return next.run(req).await;
    }

    // cookie 会话面：session 行（未过期）→ 用户（未禁用）→ 注入上下文 +
    // 滑动顺延。
    let Some(session_id) = cookie_value(req.headers(), SESSION_COOKIE_NAME) else {
        return ApiError::unauthorized().into_response();
    };
    let hash = session_id_hash(&session_id);

    let session = match state.sessions.get_valid(&hash, now).await {
        Ok(Some(session)) => session,
        Ok(None) => return ApiError::unauthorized().into_response(),
        Err(e) => return ApiError::internal("session lookup", &e).into_response(),
    };
    let user = match active_user_or_reject(&state, session.user_id).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    if let Err(e) = state.sessions.touch(&hash, now + SESSION_TTL_MS).await {
        return ApiError::internal("session touch", &e).into_response();
    }

    req.extensions_mut().insert(AuthContext {
        user_id: user.id,
        username: user.username,
        is_admin: user.is_admin,
        channel: AuthChannel::Session {
            id_hash: hash.clone(),
        },
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

/// 用户行解析（两通道共用）：按 id 取行且未禁用；缺失/禁用 401、库错 500
/// （响应形态收口在此，通道分支只管各自的凭据查行）。
async fn active_user_or_reject(
    state: &AppState,
    user_id: i64,
) -> Result<crate::store::users::User, Response> {
    match state.users.get_by_id(user_id).await {
        Ok(Some(user)) if !user.disabled => Ok(user),
        Ok(_) => Err(ApiError::unauthorized().into_response()),
        Err(e) => Err(ApiError::internal("user lookup", &e).into_response()),
    }
}

/// 解析 Bearer 凭据（RFC 7235，scheme 大小写不敏感）：Authorization 头为
/// Bearer scheme 时返回凭据串（可为任意串——查不到行即 401，不在此区分
/// 形态好坏）；其它 scheme / 头缺失返回 `None`（回落 cookie 面）。
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, credentials) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credentials.trim().to_string())
        .filter(|credentials| !credentials.is_empty())
}

/// 建号面的输入校验（setup / register / 全局 admin 建号共用）：用户名
/// 非空 + 字符集/长度，密码最小长度 8（无复杂度规则，ADR-0014）。
pub(super) fn validate_new_account(username: &str, password: &str) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if let Some(issue) = username_issue(username) {
        issues.push(issue);
    }
    if let Some(issue) = password_issue("password", password) {
        issues.push(issue);
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("凭据输入校验失败", issues))
    }
}

/// 单个新密码的校验（自助改密 / 代办重置：只验要写入的那个密码，字段
/// 路径由调用侧给——两处请求体字段名不同）。
pub(super) fn validate_new_password(path: &str, password: &str) -> Result<(), ApiError> {
    match password_issue(path, password) {
        Some(issue) => Err(ApiError::validation("密码输入校验失败", vec![issue])),
        None => Ok(()),
    }
}

/// 用户名问题项：trim 后非空、1..=64 字符、限字母数字与 `_ . -`——用户名
/// 会进 URL 路径（`/users/{name}`）与登录名，放行 `/`、空格等会让建出来
/// 的号无法寻址。空名与非法字符集合并为一条 issue（同一修正动作）。
fn username_issue(username: &str) -> Option<ValidationIssue> {
    let trimmed = username.trim();
    let charset_ok = !trimmed.is_empty()
        && trimmed.chars().count() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    (!charset_ok).then(|| ValidationIssue {
        path: "username".into(),
        message: "用户名须为 1-64 位字母、数字或 _ . -（trim 后生效）".into(),
    })
}

/// 密码问题项：最小长度 8，无复杂度规则（ADR-0014）。
fn password_issue(path: &str, password: &str) -> Option<ValidationIssue> {
    (password.chars().count() < MIN_PASSWORD_LEN).then(|| ValidationIssue {
        path: path.into(),
        message: format!("密码最小长度 {MIN_PASSWORD_LEN}（无复杂度规则）"),
    })
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

/// 登录成功审计事件（login handler 记账；时间取业务写入同一取值）。
async fn record_login_success(state: &AppState, username: &str, now: i64) -> Result<(), ApiError> {
    state
        .audit
        .insert(now, username, crate::store::audit::AuditEvent::LoginSuccess, None, None)
        .await?;
    Ok(())
}

/// 登录失败审计事件（失败即记账；用户不存在与密码错误同记，detail 无目标
/// 区分——与 401 响应形态一致）。
async fn record_login_failure(state: &AppState, username: &str, now: i64) -> Result<(), ApiError> {
    state
        .audit
        .insert(now, username, crate::store::audit::AuditEvent::LoginFailure, None, None)
        .await?;
    Ok(())
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
    fn bearer_token_parses_scheme_and_rejects_other_forms() {
        let auth = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            headers
        };

        // scheme 大小写不敏感（RFC 7235）；凭据串原样透传（查不到行即 401）。
        assert_eq!(
            bearer_token(&auth("Bearer sis_abc")).as_deref(),
            Some("sis_abc")
        );
        assert_eq!(
            bearer_token(&auth("bearer sis_abc")).as_deref(),
            Some("sis_abc")
        );

        // 非 Bearer scheme：回落 cookie 面；空凭据：不当 Bearer 处理。
        assert_eq!(bearer_token(&auth("Basic dXNlcjpwYXNz")), None);
        assert_eq!(bearer_token(&auth("Bearer")), None);
        assert_eq!(bearer_token(&auth("Bearer ")), None);
        assert_eq!(bearer_token(&auth("Bearer   ")), None);

        // 头缺失 / 非法 ASCII：None。
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn validate_new_account_enforces_username_charset_and_min_password() {
        assert!(validate_new_account("root", "12345678").is_ok());
        assert!(validate_new_account("  alice.dev-2  ", "12345678").is_ok(), "trim 后合法");
        // 长口令无复杂度要求：纯数字也过。
        assert!(validate_new_account("root", "12345678901234567890").is_ok());
        // 64 字符上限边界。
        assert!(validate_new_account(&"a".repeat(64), "12345678").is_ok());

        // 空名 / 空白名 / 非法字符（会破坏 /users/{name} 寻址）/ 超长：
        // 422 且定位 username。
        for bad in ["", "   ", "张三", "a/b", "a b", &"a".repeat(65)] {
            let err = validate_new_account(bad, "12345678").unwrap_err();
            assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
            assert_eq!(
                username_issue(bad).expect("同输入应产生 issue").path,
                "username",
                "{bad:?} 应定位 username"
            );
        }

        // 短密码：422（无复杂度规则，只钉长度）。
        let err = validate_new_account("root", "short").unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            password_issue("password", "short").expect("短密码应产生 issue").path,
            "password"
        );

        // 单密码校验的路径参数（改密请求体字段名不同）。
        assert!(validate_new_password("new_password", "12345678").is_ok());
        assert_eq!(
            validate_new_password("new_password", "short")
                .unwrap_err()
                .status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

//! 认证面进程内集成（票 B2b-T1 AC）：setup wizard、login/logout 会话、
//! GET /auth/me、认证中间件全局面 401、滑动过期与重启不掉线（Router 缝；
//! store 缝见 store::users / store::sessions 单测）。

use axum::http::{StatusCode, header};

mod common;

use common::{
    TestApp, body_json, get, post, req, req_with_cookie, setup_and_login, test_app, test_app_at,
};

const SETUP_BODY: &str = r#"{ "username": "admin", "password": "admin-password-1" }"#;
const LOGIN_BODY: &str = r#"{ "username": "admin", "password": "admin-password-1" }"#;
/// 登录勾选「保持登录」（票 #114）：remember_me=true → 30 天持久 cookie。
const LOGIN_REMEMBER_BODY: &str =
    r#"{ "username": "admin", "password": "admin-password-1", "remember_me": true }"#;

/// 读响应 Set-Cookie 原文（属性断言用）。
fn set_cookie(resp: &axum::response::Response) -> String {
    resp.headers()
        .get(header::SET_COOKIE)
        .expect("应带 Set-Cookie")
        .to_str()
        .expect("ASCII")
        .to_string()
}

async fn me(app: &TestApp, cookie: &str) -> axum::response::Response {
    req_with_cookie(app, "GET", "/api/v1/auth/me", None, Some(cookie)).await
}

/// 从 Set-Cookie 原文取 session id 值（首个 `;` 前的键值对）。
fn cookie_of_setcookie_style(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .and_then(|kv| kv.split_once('='))
        .map(|(_, v)| v.to_string())
        .expect("cookie 键值对")
}

/// 会话行快照（created_at / expires_at / remember_me，票 #114 测试用）。
struct SessionRowSnapshot {
    created_at: i64,
    expires_at: i64,
    remember_me: bool,
}

/// 按 cookie 里的 session id 值查会话行（id 哈希 = SHA-256(session_id)）。
async fn session_row(app: &TestApp, session_id: &str) -> SessionRowSnapshot {
    let id_hash = sisyphus_server::auth::session_id_hash(session_id);
    let (created_at, expires_at, remember_me): (i64, i64, i64) = sqlx::query_as(
        "SELECT created_at, expires_at, remember_me FROM sessions WHERE id_hash = ?",
    )
    .bind(&id_hash)
    .fetch_one(&app.pool)
    .await
    .expect("查 session 行");
    SessionRowSnapshot {
        created_at,
        expires_at,
        remember_me: remember_me != 0,
    }
}

/// 空库 setup 建首个全局 admin；用户表非空后一律 404（不暴露状态）。
#[tokio::test]
async fn setup_creates_first_admin_then_404s_forever() {
    let app = test_app().await;

    let resp = post(&app, "/api/v1/auth/setup", SETUP_BODY).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "空库首建应 201");
    let body = body_json(resp).await;
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true, "首个用户即全局 admin");

    // 密码哈希落库形态：argon2id PHC，明文不上库（Router 缝直查）。
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE username = 'admin'")
            .fetch_one(&app.pool)
            .await
            .expect("直查哈希");
    assert!(hash.starts_with("$argon2id$"), "PHC 形态：{hash}");
    assert!(!hash.contains("admin-password-1"), "明文不得落库");

    // 用户表非空后：对任何输入（含非法 JSON）一律 404，不借校验错误暴露状态。
    for body in [
        SETUP_BODY,
        "{ not json",
        r#"{ "username": "", "password": "x" }"#,
    ] {
        let resp = post(&app, "/api/v1/auth/setup", body).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "非空库应一律 404");
        assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
    }
}

/// setup 输入校验：密码最小 8 位、用户名非空（422 统一校验形态）。
#[tokio::test]
async fn setup_validates_min_password_and_username() {
    let app = test_app().await;

    let resp = post(
        &app,
        "/api/v1/auth/setup",
        r#"{ "username": "admin", "password": "short" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "VALIDATION_FAILED");
    let errors = body["detail"]["errors"].as_array().expect("错误清单");
    assert!(
        errors.iter().any(|e| e["path"] == "password"),
        "密码长度错误应定位到 password：{errors:?}"
    );

    let resp = post(
        &app,
        "/api/v1/auth/setup",
        r#"{ "username": "   ", "password": "long-enough-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");

    // 8 位纯数字（无复杂度规则）应通过校验。
    let resp = post(
        &app,
        "/api/v1/auth/setup",
        r#"{ "username": "ok", "password": "12345678" }"#,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "最小长度即过，无复杂度规则"
    );
}

/// login 成功换会话 cookie（属性钉死）；错密码/未知用户统一 401。
#[tokio::test]
async fn login_sets_cookie_and_wrong_credentials_unified_401() {
    let app = test_app().await;
    post(&app, "/api/v1/auth/setup", SETUP_BODY).await;

    let resp = post(&app, "/api/v1/auth/login", LOGIN_BODY).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = set_cookie(&resp);
    let body = body_json(resp).await;
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true);

    assert!(
        cookie.starts_with("sisyphus_session="),
        "cookie 名固定：{cookie}"
    );
    for attr in ["HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(attr), "缺属性 {attr}：{cookie}");
    }
    // 缺省登录（未带 remember_me）为会话级 cookie：无 Max-Age，关浏览器即失效
    //（票 #114）。
    assert!(
        !cookie.contains("Max-Age"),
        "缺省登录应无 Max-Age（会话级 cookie）：{cookie}"
    );
    assert!(!cookie.contains("Secure"), "v1 不设 Secure：{cookie}");
    let session_id = cookie
        .split(';')
        .next()
        .and_then(|kv| kv.split_once('='))
        .map(|(_, v)| v)
        .expect("cookie 键值对");
    assert_eq!(session_id.len(), 43, "32 字节 base64url 无填充");

    // 缺省会话行：7 天滑动 TTL + remember_me=0（票 #114）。
    let row = session_row(&app, session_id).await;
    assert_eq!(
        row.expires_at - row.created_at,
        sisyphus_server::auth::SESSION_TTL_MS,
        "缺省登录服务端 7 天 TTL"
    );
    assert!(!row.remember_me, "缺省登录 remember_me=false");

    // 错密码与未知用户：同一 401 形态（不区分两者存在性）。
    for body in [
        r#"{ "username": "admin", "password": "wrong-password" }"#,
        r#"{ "username": "ghost", "password": "whatever-12" }"#,
    ] {
        let resp = post(&app, "/api/v1/auth/login", body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(resp).await;
        assert_eq!(body["code"], "UNAUTHORIZED");
        assert!(
            body["message"].as_str().is_some_and(|m| !m.is_empty()),
            "message 非空：{body}"
        );
    }

    // 请求体非法 JSON：422 校验形态（与业务端点一致）。
    let resp = post(&app, "/api/v1/auth/login", "{ not json").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// remember_me=true（票 #114）：30 天持久 cookie（Max-Age=2592000）+ 服务端
/// 30 天滑动；中间件续发刷新 Max-Age 并按 30 天滑动。审计 login_success 不受
/// 影响（成功即记，与 remember_me 无关）。
#[tokio::test]
async fn login_remember_me_sets_30day_persistent_session() {
    let app = test_app().await;
    post(&app, "/api/v1/auth/setup", SETUP_BODY).await;

    let resp = post(&app, "/api/v1/auth/login", LOGIN_REMEMBER_BODY).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = set_cookie(&resp);
    assert!(
        cookie.contains("Max-Age=2592000"),
        "remember_me=true 应带 30 天 Max-Age（持久 cookie）：{cookie}"
    );
    for attr in ["HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(attr), "缺属性 {attr}：{cookie}");
    }
    let session_id = cookie_of_setcookie_style(&cookie);

    // 服务端行：30 天 TTL + remember_me=1。
    let row = session_row(&app, &session_id).await;
    assert_eq!(
        row.expires_at - row.created_at,
        sisyphus_server::auth::REMEMBER_ME_TTL_MS,
        "remember_me=true 服务端 30 天 TTL"
    );
    assert!(row.remember_me, "remember_me=true 应落库");

    // 审计 login_success 已记（与 remember_me 无关）。
    let login_audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_type = 'login_success'")
            .fetch_one(&app.pool)
            .await
            .expect("查审计");
    assert_eq!(login_audits, 1, "登录成功应记一次 login_success");

    // 中间件续发：认证响应刷新 30 天 Max-Age + 行按 30 天滑动。
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let resp = me(&app, &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let renewed = set_cookie(&resp);
    assert!(
        renewed.contains("Max-Age=2592000"),
        "持久会话续发应刷新 30 天 Max-Age：{renewed}"
    );
    let row_after = session_row(&app, &session_id).await;
    assert!(
        row_after.expires_at > row.expires_at,
        "持久会话应按 30 天滑动：{} -> {}",
        row.expires_at,
        row_after.expires_at
    );
}

/// GET /auth/me：带 cookie 返回当前用户；无/坏 cookie 401。
#[tokio::test]
async fn me_returns_current_user_or_401() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    let resp = me(&app, &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true);

    // 无 cookie / 库里无行的 cookie：统一 401。
    let resp = get(&app, "/api/v1/auth/me").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");

    let resp = me(
        &app,
        "sisyphus_session=AAAA-not-a-real-session-id-value-00000000",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// logout 删会话行：原 cookie 即刻失效，响应清空 cookie。
#[tokio::test]
async fn logout_invalidates_cookie_immediately() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    let resp = req_with_cookie(&app, "POST", "/api/v1/auth/logout", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // 恰一个 Set-Cookie：logout 的清空头不被中间件续发头覆盖。
    assert_eq!(
        resp.headers().get_all(header::SET_COOKIE).iter().count(),
        1,
        "logout 响应应只有清空 cookie"
    );
    let clearing = set_cookie(&resp);
    assert!(
        clearing.starts_with("sisyphus_session="),
        "清空形：{clearing}"
    );
    assert!(clearing.contains("Max-Age=0"), "即刻过期：{clearing}");

    // 原 cookie 再访问：401（行已删）。
    let resp = me(&app, &cookie).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 未认证 logout：401。
    let resp = post(&app, "/api/v1/auth/logout", "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// 认证中间件全局面：未认证访问业务端点一律 401；放行面不拦。
#[tokio::test]
async fn business_endpoints_require_auth_but_public_surface_open() {
    let app = test_app().await;

    // 现有 projects/pipelines 端点与一切业务端点：未认证 401 统一形态。
    for (method, path, body) in [
        ("GET", "/api/v1/projects", None),
        ("POST", "/api/v1/projects", Some(r#"{ "name": "x" }"#)),
        ("GET", "/api/v1/projects/demo", None),
        ("GET", "/api/v1/projects/demo/pipelines/build", None),
        (
            "PUT",
            "/api/v1/projects/demo/pipelines/build",
            Some(r#"{ "name": "build", "stages": [] }"#),
        ),
        ("GET", "/api/v1/auth/me", None),
        ("POST", "/api/v1/auth/logout", None),
    ] {
        let resp = req(&app, method, path, body.map(Into::into)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} 未认证应 401"
        );
        assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
    }

    // 放行面：healthz 与静态资源不经认证（healthz 不在 /api/v1 下；
    // 静态面走根 fallback）。
    let resp = get(&app, "/healthz").await;
    assert_eq!(resp.status(), StatusCode::OK);
    // 静态面放行：往覆盖目录放一个 index.html 作探针（不依赖前端构建产物
    // ——内嵌产物面由 static_web/web_handshake 在注入真实 dist 后覆盖）；
    // 认证中间件不得拦静态资源面。
    std::fs::create_dir_all(&app.web).unwrap();
    std::fs::write(
        app.web.join("index.html"),
        "<!doctype html><p>static-ok</p>",
    )
    .unwrap();
    let resp = get(&app, "/").await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "静态资源面（SPA fallback）不拦"
    );

    // login/setup 本身未认证可达（上面用例已证）。

    // 登录后业务端点放行：全局面 401 闭环。
    let cookie = setup_and_login(&app).await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "认证后放行");
}

/// 滑动过期（Router 缝）：认证通过即顺延；过期 session 认证失败。
#[tokio::test]
async fn session_slides_on_auth_and_expired_fails() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let session_id_hash = {
        let id = cookie_of_setcookie_style(&cookie);
        sisyphus_server::auth::session_id_hash(&id)
    };

    // 登录后的初始过期时间。
    let expires_before: i64 =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id_hash = ?")
            .bind(&session_id_hash)
            .fetch_one(&app.pool)
            .await
            .expect("读 session 行");

    // 认证通过即顺延：一次 /auth/me 后过期时间被推进（隔几毫秒，避开
    // 毫秒时间戳同值）。
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let resp = me(&app, &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK);
    // 浏览器侧同步滑动：认证响应续发同值 cookie。缺省（未勾选 remember_me）
    // 为会话级 cookie，续发不带 Max-Age（票 #114）。
    let renewed = set_cookie(&resp);
    assert!(renewed.starts_with(&cookie), "续发同值 cookie：{renewed}");
    assert!(
        !renewed.contains("Max-Age"),
        "会话级会话续发不带 Max-Age：{renewed}"
    );
    let expires_after: i64 =
        sqlx::query_scalar("SELECT expires_at FROM sessions WHERE id_hash = ?")
            .bind(&session_id_hash)
            .fetch_one(&app.pool)
            .await
            .expect("再读 session 行");
    assert!(
        expires_after > expires_before,
        "认证通过应顺延过期：{expires_before} -> {expires_after}"
    );

    // 把行改到过去：过期 session 认证失败。
    sqlx::query("UPDATE sessions SET expires_at = 1 WHERE id_hash = ?")
        .bind(&session_id_hash)
        .execute(&app.pool)
        .await
        .expect("直改过期时间");
    let resp = me(&app, &cookie).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "过期 session 应 401"
    );
}

/// 重启不掉线（Router 缝）：同一数据目录重开装配，旧 cookie 仍有效。
#[tokio::test]
async fn session_survives_router_reassembly() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    sisyphus_server::config::Config::load(
        dir.path().to_path_buf(),
        Default::default(),
        Default::default(),
    )
    .expect("目录布局");

    let app1 = test_app_at(dir.path()).await;
    let cookie = setup_and_login(&app1).await;
    let resp = me(&app1, &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK, "重启前应已登录");

    // 模拟 Server 重启：同一数据目录重新 bootstrap + 新 Router。
    let app2 = test_app_at(dir.path()).await;
    let resp = me(&app2, &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK, "重启后 session 仍有效");
}

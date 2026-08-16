//! 认证加固面进程内集成（票 B2b-T2 AC）：登录限流（per-IP / per-username
//! 双键、429、重启即清）与 CSRF 中间件（跨源拒、同源过、GET 不拦、双头
//! 皆缺拒、Bearer 免疫）。冷却时长递增/封顶与成功清零是限流器假时钟内联
//! 单测（`sisyphus_server::auth`）；本文件只测 Router 缝的可观察行为。

use axum::http::{StatusCode, header};
use axum::response::Response;
use std::net::SocketAddr;

mod common;

use common::{
    TestApp, body_json, custom_req, post, req_with_cookie, setup_and_login, test_app, test_app_at,
};

const SETUP_BODY: &str = r#"{ "username": "admin", "password": "admin-password-1" }"#;
const LOGIN_BODY: &str = r#"{ "username": "admin", "password": "admin-password-1" }"#;
const WRONG_LOGIN_BODY: &str = r#"{ "username": "admin", "password": "wrong-password" }"#;

/// 指定端口的直连地址（限流键只取 IP；同 IP 换端口不算换来源）。
fn peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// 与 [`peer`] 不同 IP 的直连地址（127.0.0.2，驱动 per-IP 键的真隔离）。
fn other_ip_peer(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 2], port))
}

/// 从指定直连地址发登录请求。
async fn login_from(app: &TestApp, body: &str, peer: SocketAddr) -> Response {
    custom_req(
        app,
        "POST",
        "/api/v1/auth/login",
        Some(body.into()),
        None,
        &[],
        peer,
    )
    .await
}

/// 同源标头组（Host 基准 ci.local + 与之同源的 Origin）。
fn same_origin_headers() -> Vec<(&'static str, String)> {
    vec![
        ("host", "ci.local".to_string()),
        ("origin", "http://ci.local".to_string()),
    ]
}

/// 跨源标头组（Origin 指向外部站点）。
fn cross_origin_headers() -> Vec<(&'static str, String)> {
    vec![
        ("host", "ci.local".to_string()),
        ("origin", "https://evil.example".to_string()),
    ]
}

/// 已认证（cookie）的全自定义请求。
async fn authed(
    app: &TestApp,
    method: &str,
    path: &str,
    body: Option<&str>,
    cookie: &str,
    headers: &[(&str, String)],
) -> Response {
    custom_req(
        app,
        method,
        path,
        body.map(Into::into),
        Some(cookie),
        headers,
        peer(9),
    )
    .await
}

// ---------------------------------------------------------------------------
// 登录限流
// ---------------------------------------------------------------------------

/// 连续 5 次失败后，第 6 次即使密码正确也 429（统一错误形态 + Retry-After）。
#[tokio::test]
async fn sixth_login_attempt_is_429_even_with_correct_password() {
    let app = test_app().await;
    post(&app, "/api/v1/auth/setup", SETUP_BODY).await;

    for i in 1..=5 {
        let resp = login_from(&app, WRONG_LOGIN_BODY, peer(1)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "第 {i} 次失败应 401"
        );
    }

    let resp = login_from(&app, LOGIN_BODY, peer(1)).await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "正确密码也应 429"
    );
    let retry_after = resp
        .headers()
        .get(header::RETRY_AFTER)
        .expect("429 应带 Retry-After")
        .to_str()
        .expect("ASCII")
        .to_string();
    let body = body_json(resp).await;
    assert_eq!(body["code"], "RATE_LIMITED");
    assert!(
        body["detail"]["retry_after_ms"]
            .as_i64()
            .is_some_and(|ms| ms > 0),
        "detail 携带剩余毫秒：{body}"
    );
    assert!(
        retry_after.parse::<u64>().is_ok_and(|secs| secs > 0),
        "Retry-After 为正整秒：{retry_after}"
    );
}

/// per-username 键：换 IP 也拦（撞库同一账号被拖慢，与来源无关）。
#[tokio::test]
async fn per_username_cooldown_blocks_login_from_a_different_ip() {
    let app = test_app().await;
    post(&app, "/api/v1/auth/setup", SETUP_BODY).await;

    for _ in 0..5 {
        login_from(&app, WRONG_LOGIN_BODY, peer(1)).await;
    }
    // 换一个真正不同的来源 IP（限流键只取 IP，与端口无关，须换地址）：
    // ip 键全新，用户名键仍在冷却——正确密码也 429。
    let resp = login_from(&app, LOGIN_BODY, other_ip_peer(2)).await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "用户名键应跨 IP 拦截"
    );
}

/// per-IP 键：同一来源换用户名也拦（暴破用户名字典被拖慢）。
#[tokio::test]
async fn per_ip_cooldown_blocks_other_usernames_from_same_ip() {
    let app = test_app().await;
    post(&app, "/api/v1/auth/setup", SETUP_BODY).await;

    // 以不存在的用户名失败 5 次（每次双键各记一笔：ip 键累计满 5）。
    for _ in 0..5 {
        let resp = login_from(
            &app,
            r#"{ "username": "ghost", "password": "whatever-12" }"#,
            peer(3),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    // 同 IP（端口不同不算换 IP）换真实账号 + 正确密码：ip 键在冷却——429。
    let resp = login_from(&app, LOGIN_BODY, peer(4)).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// 限流为进程内状态：同数据目录重开装配（Server 重启缝）即清，无持久锁定。
#[tokio::test]
async fn rate_limit_state_clears_on_reassembly() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    sisyphus_server::config::Config::load(
        dir.path().to_path_buf(),
        Default::default(),
        Default::default(),
    )
    .expect("目录布局");

    let app1 = test_app_at(dir.path()).await;
    post(&app1, "/api/v1/auth/setup", SETUP_BODY).await;
    for _ in 0..5 {
        login_from(&app1, WRONG_LOGIN_BODY, peer(4)).await;
    }
    let resp = login_from(&app1, LOGIN_BODY, peer(4)).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS, "重启前应冷却");

    // 模拟 Server 重启：同一数据目录重新 bootstrap + 新 Router（新 AppState，
    // 限流器随进程内状态清零）——同来源同账号立即可登。
    let app2 = test_app_at(dir.path()).await;
    let resp = login_from(&app2, LOGIN_BODY, peer(4)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "重启后限流即清（无持久锁定）"
    );
}

// ---------------------------------------------------------------------------
// CSRF 中间件
// ---------------------------------------------------------------------------

/// cookie 认证的非安全方法请求：跨源 Origin 拒（403 CSRF_REJECTED）；GET 不拦。
#[tokio::test]
async fn csrf_rejects_cross_site_unsafe_methods_but_not_get() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 跨源 POST：403，且发生在 handler 之前（logout 未执行，会话仍活）。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/auth/logout",
        None,
        &cookie,
        &cross_origin_headers(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_REJECTED");

    // 跨源 PUT（pipeline 定义面）：同样 403，先于 handler（无需项目存在）。
    let resp = authed(
        &app,
        "PUT",
        "/api/v1/projects/demo/pipelines/build",
        Some(r#"{ "name": "build", "stages": [] }"#),
        &cookie,
        &cross_origin_headers(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Sec-Fetch-Site 非同源（cross-site）：拒。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/auth/logout",
        None,
        &cookie,
        &[("sec-fetch-site", "cross-site".to_string())],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // GET 是安全方法：即便跨源也不拦。
    let resp = authed(
        &app,
        "GET",
        "/api/v1/auth/me",
        None,
        &cookie,
        &cross_origin_headers(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "GET 不在 CSRF 检查面");

    // logout 未被执行（跨源那次被拦在 handler 之前）：会话仍有效。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 同源凭证（Origin 同源 / Sec-Fetch-Site same-origin / same-site）放行。
#[tokio::test]
async fn csrf_allows_same_origin_requests() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let create = |name: &str| {
        format!(
            r#"{{ "name": "{name}", "scm_type": "git", "scm_url": "https://example.com/{name}" }}"#
        )
    };

    // Origin 与 Host 同源：POST 建 项目成功（201 证明完整放行，非仅非 403）。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(&create("csrf-origin")),
        &cookie,
        &same_origin_headers(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "同源 Origin 应放行");

    // 无 Origin、Sec-Fetch-Site: same-origin：放行。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(&create("csrf-fetch-origin")),
        &cookie,
        &[("sec-fetch-site", "same-origin".to_string())],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "same-origin 应放行");

    // Sec-Fetch-Site: same-site（同站子域）：放行。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(&create("csrf-fetch-site")),
        &cookie,
        &[("sec-fetch-site", "same-site".to_string())],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "same-site 应放行");
}

/// Origin 与 Sec-Fetch-Site 双头皆缺的 cookie 认证请求同样拒；无效 cookie
/// 的请求先被认证拦（层序：认证 401 在外层）；公开段 login 不经 CSRF。
#[tokio::test]
async fn csrf_rejects_missing_headers_but_auth_runs_first() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 双头皆缺：403（浏览器必带其一；非浏览器脚本走 PAT）。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(r#"{ "name": "x", "scm_type": "git", "scm_url": "https://e.com/x" }"#),
        &cookie,
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["code"], "CSRF_REJECTED");

    // 无效 cookie + 双头皆缺：认证中间件在外层先拒——401 而非 403
    // （「以 cookie 认证」指已过认证的请求）。
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(r#"{ "name": "y", "scm_type": "git", "scm_url": "https://e.com/y" }"#),
        "sisyphus_session=AAAA-not-a-real-session-000000000000000000",
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "认证先于 CSRF");

    // 公开段（login）不挂 CSRF：携陈旧 cookie、无同源凭证的登录请求
    // 正常进入凭据校验（401 是凭据错，不是 CSRF 拒）。
    let resp = custom_req(
        &app,
        "POST",
        "/api/v1/auth/login",
        Some(WRONG_LOGIN_BODY.into()),
        Some("sisyphus_session=stale"),
        &[],
        peer(8),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "login 不在 CSRF 面"
    );
}

/// Bearer 请求不经 CSRF 检查（PAT 面）：无效 PAT 是认证 401（轮不到 CSRF
/// 拒）；有效 PAT（T3 落地，经 /auth/tokens 签发）跨源照常 201——免疫的
/// 完整生命周期见 tests/pat_auth.rs。
#[tokio::test]
async fn bearer_requests_are_immune_to_csrf() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 纯 Bearer（无 cookie）：跨源 + 双头皆缺都轮不到 CSRF 拒——认证 401。
    let bearer = |origin: Option<&str>| {
        let mut headers = vec![("authorization", "Bearer sis_not-a-token-yet".to_string())];
        if let Some(origin) = origin {
            headers.push(("origin", origin.to_string()));
        }
        headers
    };
    for headers in [bearer(Some("https://evil.example")), bearer(None)] {
        let resp = custom_req(
            &app,
            "POST",
            "/api/v1/projects",
            Some(r#"{ "name": "z", "scm_type": "git", "scm_url": "https://e.com/z" }"#.into()),
            None,
            &headers,
            peer(7),
        )
        .await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN, "Bearer 免疫 CSRF");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // cookie + Bearer 并存：按显式凭据对待（T3 起 Bearer 优先于 cookie）。
    // 无效 PAT 失败关闭——401 是认证拒绝（轮不到 CSRF），不回落 cookie 面。
    let mut headers = cross_origin_headers();
    headers.push(("authorization", "Bearer sis_not-a-token-yet".to_string()));
    let resp = authed(
        &app,
        "POST",
        "/api/v1/projects",
        Some(r#"{ "name": "w", "scm_type": "git", "scm_url": "https://e.com/w" }"#),
        &cookie,
        &headers,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "无效显式凭据失败关闭"
    );

    // 有效 PAT + 跨源 Origin：显式凭据跳过 CSRF 面，201 落地。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "csrf-proof" }"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "签发真 PAT");
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("创建响应带 token")
        .to_string();
    let mut headers = cross_origin_headers();
    headers.push(("authorization", format!("Bearer {token}")));
    let resp = custom_req(
        &app,
        "POST",
        "/api/v1/projects",
        Some(r#"{ "name": "w", "scm_type": "git", "scm_url": "https://e.com/w" }"#.into()),
        None,
        &headers,
        peer(9),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "有效 PAT 跳过 CSRF 面");
}

//! PAT 与 Bearer 认证进程内集成（票 B2b-T3 AC，Router 缝）：创建一次性
//! 回显 / 列表无值形态、Bearer 与 cookie 同权重放全部业务端点、格式错 /
//! 吊销 / 过期 / 属主被禁一律 401、Bearer 免 CSRF、登出通道语义。
//! store 缝（哈希落库、唯一约束、吊销删行）见 store::tokens 单测。

use axum::http::{StatusCode, header};
use axum::response::Response;
use sisyphus_server::auth::{TokenFamily, generate_token, token_hash};

mod common;

use common::{TestApp, body_json, body_text, req_with_cookie, setup_and_login, test_app};

/// oneshot 请求直连地址（与 common::DEFAULT_PEER 同形）。
const PEER: std::net::SocketAddr = common::DEFAULT_PEER;

/// Bearer 认证的进程内请求（无 cookie、无 CSRF 凭证——Bearer 面天然免疫，
/// 有意不带任何同源头，同时钉住这一点）。
async fn bearer(
    app: &TestApp,
    method: &str,
    path: &str,
    body: Option<String>,
    token: &str,
) -> Response {
    common::custom_req(
        app,
        method,
        path,
        body,
        None,
        &[("authorization", format!("Bearer {token}"))],
        PEER,
    )
    .await
}

/// 登录并以 cookie 创建一枚 PAT，返回 (cookie, 明文 token, 行 id)。
async fn mint_pat(app: &TestApp, name: &str, expires_at: Option<i64>) -> (String, String, i64) {
    let cookie = setup_and_login(app).await;
    let body = serde_json::json!({ "name": name, "expires_at": expires_at }).to_string();
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/auth/tokens",
        Some(body),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "创建 PAT 应 201");
    let body = body_json(resp).await;
    let token = body["token"].as_str().expect("token 字段").to_string();
    (cookie, token, body["id"].as_i64().expect("id 字段"))
}

/// 响应 Set-Cookie 是否存在。
fn has_set_cookie(resp: &Response) -> bool {
    resp.headers().contains_key(header::SET_COOKIE)
}

/// 创建响应一次性返回 `sis_` 前缀完整令牌；列表与后续任何响应不再出现值
/// （票 B2b-T3 AC1）。
#[tokio::test]
async fn create_returns_full_token_once_and_list_never_exposes_value() {
    let app = test_app().await;
    let (cookie, token, id) = mint_pat(&app, "ci-deploy", None).await;

    assert!(token.starts_with("sis_"), "族前缀：{token}");
    assert_eq!(token.len(), 47, "前缀 + 43 字符：{token}");
    assert!(
        token["sis_".len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "URL 安全字母表：{token}"
    );

    // 列表：只有名/时间/过期，无值形态。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/tokens", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let text = body_text(resp).await;
    assert!(!text.contains(&token), "列表不得出现令牌值：{text}");
    assert!(!text.contains("token_hash"), "哈希同样不出 API 面：{text}");
    let list: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let entries = list.as_array().expect("数组");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], id);
    assert_eq!(entries[0]["name"], "ci-deploy");
    assert_eq!(entries[0]["expires_at"], serde_json::Value::Null);
    assert!(entries[0].get("token").is_none(), "列表项不得有 token 字段");

    // Router 缝直查临时库（票 B2b-T3 AC3）：库里只有 SHA-256，明文不落库。
    let stored: Option<String> =
        sqlx::query_scalar("SELECT token_hash FROM personal_access_tokens WHERE id = ?")
            .bind(id)
            .fetch_optional(&app.pool)
            .await
            .expect("直查 token_hash");
    assert_eq!(stored.as_deref(), Some(token_hash(&token).as_str()));
    assert_ne!(stored.as_deref(), Some(token.as_str()));

    // 吊销响应亦无值形态（204 无体）。
    let resp = req_with_cookie(
        &app,
        "DELETE",
        &format!("/api/v1/auth/tokens/{id}"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// Bearer PAT 可调用全部业务端点，与 cookie 等价（票 B2b-T3 AC2）；不带
/// 任何 CSRF 凭证照常可用（Bearer 免疫，脚本与 CI 无需处理 cookie/同源头）。
#[tokio::test]
async fn bearer_pat_calls_all_business_endpoints_equivalent_to_cookie() {
    let app = test_app().await;
    let (_cookie, token, _id) = mint_pat(&app, "ci", None).await;

    // /auth/me：同权重放（身份 = PAT 属主本人）。
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true);

    // 全部业务端点走一遍：建项目 → 列表 → 读单项目 → PUT 定义 → 读定义。
    let resp = bearer(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/repo" }"#
                .into(),
        ),
        &token,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "Bearer 建项目（无 CSRF 凭证）"
    );

    let resp = bearer(&app, "GET", "/api/v1/projects", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body.as_array().expect("数组").len(), 1);

    let resp = bearer(&app, "GET", "/api/v1/projects/demo", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let definition = r#"{ "name": "build", "stages": [{
        "name": "build",
        "jobs": [{ "name": "compile", "steps": [
            { "type": "shell", "config": { "command": "cargo build" } }
        ] }]
    }] }"#;
    let resp = bearer(
        &app,
        "PUT",
        "/api/v1/projects/demo/pipelines/build",
        Some(definition.into()),
        &token,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "Bearer 保存定义（首存 200）");
    let resp = bearer(
        &app,
        "GET",
        "/api/v1/projects/demo/pipelines/build",
        None,
        &token,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "Bearer 读定义");

    // PAT 管理端点自身也走 Bearer（用旧 PAT 创建新 PAT）。
    let resp = bearer(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "another" }"#.into()),
        &token,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // 跨源头也不拦：显式凭据无 CSRF 面。
    let resp = common::custom_req(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "cross" }"#.into()),
        None,
        &[
            ("authorization", format!("Bearer {token}")),
            ("origin", "https://evil.example".to_string()),
            ("host", "ci.local".to_string()),
        ],
        PEER,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "Bearer 不经 CSRF 检查");
}

/// 格式错 / 吊销 / 过期的 token 一律 401（票 B2b-T3 AC2/AC4）；认证中间件
/// 的通道选择：Bearer scheme 优先于 cookie（显式凭据），非 Bearer scheme
/// 回落 cookie 面。
#[tokio::test]
async fn bearer_401_matrix_and_channel_precedence() {
    let app = test_app().await;
    let (cookie, token, id) = mint_pat(&app, "matrix", None).await;

    // 基线：有效 PAT 通过。
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 格式错：无前缀乱串 / Agent 族值（两族不混用）/ 空凭据——查不到行
    // 一律 401，与吊销/过期同形态（不区分原因）。
    for bad in [
        "garbage-token-without-prefix",
        &generate_token(TokenFamily::Agent),
        "sis_short",
    ] {
        let resp = bearer(&app, "GET", "/api/v1/auth/me", None, bad).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad} 应 401");
        assert_eq!(body_json(resp).await["code"], "UNAUTHORIZED");
    }

    // Bearer 优先于 cookie：有效 cookie + 无效 Bearer 并存 → 按显式凭据
    // 对待，401（不回落 cookie）。
    let resp = common::custom_req(
        &app,
        "GET",
        "/api/v1/auth/me",
        None,
        Some(&cookie),
        &[("authorization", "Bearer not-a-token".to_string())],
        PEER,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "Bearer 优先于 cookie"
    );

    // 非 Bearer scheme（Basic 等）：回落 cookie 面——有效 cookie 仍可用。
    let resp = common::custom_req(
        &app,
        "GET",
        "/api/v1/auth/me",
        None,
        Some(&cookie),
        &[("authorization", "Basic dXNlcjpwYXNz".to_string())],
        PEER,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "非 Bearer scheme 回落 cookie"
    );

    // 吊销后立即失效（票 B2b-T3 AC4）：DELETE 删行，下一请求即 401。
    let resp = req_with_cookie(
        &app,
        "DELETE",
        &format!("/api/v1/auth/tokens/{id}"),
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "吊销后应立即失效");
}

/// 带过期的 PAT 到期后认证失败（票 B2b-T3 AC4）：直改库把过期时间推到
/// 过去（不睡时钟）。
#[tokio::test]
async fn expired_pat_fails_auth() {
    let app = test_app().await;
    let far_future = 4_102_444_800_000_i64; // 2100-01-01，建时合法。
    let (_cookie, token, id) = mint_pat(&app, "bounded", Some(far_future)).await;

    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "未过期应通过");

    sqlx::query("UPDATE personal_access_tokens SET expires_at = 1 WHERE id = ?")
        .bind(id)
        .execute(&app.pool)
        .await
        .expect("直改过期时间");

    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "过期后应 401");
}

/// 属主被禁用：PAT 通道同样拒绝（禁用的爆炸半径含 PAT，踢线级联删行随
/// 用户管理批次，认证面先行兜底）。
#[tokio::test]
async fn disabled_user_fails_pat_auth() {
    let app = test_app().await;
    let (_cookie, token, _id) = mint_pat(&app, "doomed-owner", None).await;

    sqlx::query("UPDATE users SET disabled = 1 WHERE username = 'admin'")
        .execute(&app.pool)
        .await
        .expect("直改禁用标志");

    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "属主被禁应 401");
}

/// 通道语义：PAT 认证的响应不续发会话 cookie（无会话可滑）；Bearer 登出
/// 无事可做（204 无 Set-Cookie、PAT 仍有效、既有会话不受影响）；cookie
/// 登出照旧删行清 cookie，PAT 不受牵连。
#[tokio::test]
async fn channel_semantics_no_cookie_renewal_and_logout_split() {
    let app = test_app().await;
    let (cookie, token, _id) = mint_pat(&app, "channels", None).await;

    // PAT 认证响应不续发 cookie（与 cookie 通道的滑动续发形成对照）。
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!has_set_cookie(&resp), "Bearer 通道不应下发会话 cookie");

    // Bearer 登出：204、无 Set-Cookie、PAT 与既有会话都仍在。
    let resp = bearer(&app, "POST", "/api/v1/auth/logout", None, &token).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(!has_set_cookie(&resp), "Bearer 登出无 cookie 可清");
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "PAT 不受 Bearer 登出影响");
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "会话不受 Bearer 登出影响");

    // cookie 登出：删行清 cookie；PAT 是独立凭据，不受牵连。
    let resp = req_with_cookie(&app, "POST", "/api/v1/auth/logout", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(has_set_cookie(&resp), "cookie 登出应清 cookie");
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "登出后会话失效");
    let resp = bearer(&app, "GET", "/api/v1/auth/me", None, &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "PAT 不受会话登出影响");
}

/// 创建/吊销端点的校验与错误形态：422 定位路径、404 统一 JSON、401 面。
#[tokio::test]
async fn create_and_revoke_error_surface() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 未认证：三端点一律 401。
    for (method, path) in [
        ("GET", "/api/v1/auth/tokens"),
        ("POST", "/api/v1/auth/tokens"),
        ("DELETE", "/api/v1/auth/tokens/1"),
    ] {
        let resp = common::req(&app, method, path, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    // 名空白 / 过去过期时间：422 且定位到字段。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "   " }"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "VALIDATION_FAILED");
    assert!(
        body["detail"]["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|e| e["path"] == "name")),
        "应定位到 name：{body}"
    );

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "ok", "expires_at": 1 }"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert!(
        body["detail"]["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|e| e["path"] == "expires_at")),
        "应定位到 expires_at：{body}"
    );

    // 非法 JSON：422。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some("{ not json".into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // 带合法过期时间创建：列表回显 expires_at。
    let far_future = 4_102_444_800_000_i64;
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(serde_json::json!({ "name": "bounded", "expires_at": far_future }).to_string()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["expires_at"], far_future);

    // 吊销不存在 / 非数字 id：统一 JSON 404（不落 axum 纯文本拒绝）。
    for id in ["999999", "not-a-number"] {
        let resp = req_with_cookie(
            &app,
            "DELETE",
            &format!("/api/v1/auth/tokens/{id}"),
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "id={id}");
        assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
    }
}

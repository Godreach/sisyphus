//! 用户管理与注册开关进程内集成（票 B2b-T4 AC，Router 缝）：全局 admin
//! 建/列/禁用/启用/代办重置，非全局 admin 一律 403，禁用同秒踢线（session
//! 与 PAT 下一请求即 401）、启用后可重新登录且历史行不动，自改密需验当前
//! 密码，注册开关默认关 403 / 任一层打开后自注册成功且为非管理员。
//! store 缝（级联删行、密码覆写、列表）见 store::users 单测；config 三层
//! 合并链见 config 单测。

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie, test_app, test_app_with};

/// 建号端点统一用的测试密码（setup admin 的密码由 setup_and_login 自带）。
const USER_PASSWORD: &str = "user-password-12";

/// 直连登录（公开段，无 cookie/CSRF 面），返回原始响应。
async fn login_raw(app: &TestApp, username: &str, password: &str) -> Response {
    common::post(
        app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "{username}", "password": "{password}" }}"#),
    )
    .await
}

/// 登录并断言成功，返回会话 cookie。
async fn login(app: &TestApp, username: &str, password: &str) -> String {
    let resp = login_raw(app, username, password).await;
    assert_eq!(resp.status(), StatusCode::OK, "login {username} 应成功");
    cookie_of(&resp).expect("会话 cookie")
}

/// 全局 admin 经建号端点建一个普通用户（隐含断言 201）。
async fn create_user(app: &TestApp, admin: &str, username: &str, password: &str) -> Value {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/users",
        Some(format!(
            r#"{{ "username": "{username}", "password": "{password}" }}"#
        )),
        Some(admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建号 {username} 应 201");
    body_json(resp).await
}

/// 全局 admin 建 + 目标用户登录 + 目标用户建 PAT，返回 (会话 cookie, PAT 明文)。
/// 禁用踢线用例的凭据面预置。
async fn user_with_credentials(app: &TestApp, admin: &str, username: &str) -> (String, String) {
    create_user(app, admin, username, USER_PASSWORD).await;
    let cookie = login(app, username, USER_PASSWORD).await;
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "ci" }"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建 PAT 应 201");
    let body = body_json(resp).await;
    (cookie, body["token"].as_str().expect("token").to_string())
}

/// Bearer 请求（无任何同源凭证——Bearer 面免疫 CSRF）。
async fn bearer_get(app: &TestApp, path: &str, token: &str) -> Response {
    common::custom_req(
        app,
        "GET",
        path,
        None,
        None,
        &[("authorization", format!("Bearer {token}"))],
        common::DEFAULT_PEER,
    )
    .await
}

/// 建/列：默认非管理员、可选 is_admin、重名 409、列表含禁用行且无哈希形态。
#[tokio::test]
async fn admin_creates_and_lists_users() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;

    let alice = create_user(&app, &admin, "alice", USER_PASSWORD).await;
    assert_eq!(alice["username"], "alice");
    assert_eq!(alice["is_admin"], false, "默认建普通用户");
    assert_eq!(alice["disabled"], false);
    assert!(alice.get("password_hash").is_none(), "哈希不出 API 面");

    // 显式建 admin。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/users",
        Some(r#"{ "username": "deputy", "password": "deputy-pass-123", "is_admin": true }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["is_admin"], true);

    // 重名：409。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/users",
        Some(format!(r#"{{ "username": "alice", "password": "{USER_PASSWORD}" }}"#)),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // 列表：admin 自身 + alice + deputy，按用户名排序。
    let resp = req_with_cookie(&app, "GET", "/api/v1/users", None, Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("数组")
        .iter()
        .map(|u| u["username"].as_str().expect("username"))
        .collect();
    assert_eq!(names, vec!["admin", "alice", "deputy"]);
    assert!(
        !list.to_string().contains("password_hash"),
        "列表无哈希形态：{list}"
    );
}

/// 票 B2b-T4 AC：非全局 admin 访问用户管理端点 403；未认证 401。
#[tokio::test]
async fn user_management_requires_global_admin() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_user(&app, &admin, "alice", USER_PASSWORD).await;
    let alice = login(&app, "alice", USER_PASSWORD).await;

    let cases: &[(&str, &str, &str)] = &[
        ("GET", "/api/v1/users", ""),
        ("POST", "/api/v1/users", r#"{ "username": "eve", "password": "eve-password-12" }"#),
        ("PATCH", "/api/v1/users/alice", r#"{ "disabled": true }"#),
        ("PUT", "/api/v1/users/alice/password", r#"{ "new_password": "whatever-123" }"#),
    ];
    for (method, path, body) in cases {
        // 未认证：401。
        let resp = common::req(&app, method, path, (!body.is_empty()).then(|| body.to_string())).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "未认证 {method} {path}");

        // 已认证非 admin：403。
        let resp = req_with_cookie(
            &app,
            method,
            path,
            (!body.is_empty()).then(|| body.to_string()),
            Some(&alice),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "非 admin {method} {path}");
    }
}

/// 票 B2b-T4 AC 核心：禁用后 session 与 PAT 立即全部失效（下一请求 401）、
/// 启用后可重新登录、历史数据不动（用户行保留、原密码不变）。
#[tokio::test]
async fn disable_kicks_sessions_and_pats_and_enable_allows_relogin() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;
    let (alice_cookie, pat) = user_with_credentials(&app, &admin, "alice").await;

    // 基线：两通道都可用。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&alice_cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = bearer_get(&app, "/api/v1/auth/me", &pat).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 禁用：200 且响应行 disabled=true。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/users/alice",
        Some(r#"{ "disabled": true }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["disabled"], true);
    assert_eq!(body["username"], "alice");

    // 同秒踢线：session 与 PAT 下一请求即 401。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&alice_cookie)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "禁用后 session 应 401");
    let resp = bearer_get(&app, "/api/v1/auth/me", &pat).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "禁用后 PAT 应 401");

    // 禁用中不可登录。
    let resp = login_raw(&app, "alice", USER_PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "禁用中登录应拒");

    // 历史数据不动：行仍在（列表可见且 disabled=true），管理员自身不受牵连。
    let resp = req_with_cookie(&app, "GET", "/api/v1/users", None, Some(&admin)).await;
    let list = body_json(resp).await;
    let alice_row = list
        .as_array()
        .expect("数组")
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("禁用行仍在列表");
    assert_eq!(alice_row["disabled"], true);

    // 启用：原密码可重新登录（新会话）。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/users/alice",
        Some(r#"{ "disabled": false }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["disabled"], false);
    let new_cookie = login(&app, "alice", USER_PASSWORD).await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&new_cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "启用后原密码可登录");

    // 旧 PAT 不复活（启用不重建凭据，需重新签发）。
    let resp = bearer_get(&app, "/api/v1/auth/me", &pat).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "禁用期删掉的 PAT 不复活");
}

/// 票 B2b-T4 AC：代办重置后旧密码失效、新密码可登录；未知用户 404。
#[tokio::test]
async fn admin_reset_password_replaces_credentials() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_user(&app, &admin, "alice", USER_PASSWORD).await;
    let alice_cookie = login(&app, "alice", USER_PASSWORD).await;

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/users/alice/password",
        Some(r#"{ "new_password": "rotated-pass-12" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "重置应 204");

    // 旧密码失效、新密码可登录。
    let resp = login_raw(&app, "alice", USER_PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "旧密码应失效");
    let resp = login_raw(&app, "alice", "rotated-pass-12").await;
    assert_eq!(resp.status(), StatusCode::OK, "新密码应可登录");

    // 既有会话不受牵连（密码与凭据是两个失效面，各自有吊销途径）。
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&alice_cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK, "重置不踢既有会话");

    // 未知用户：404；短密码：422。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/users/nobody/password",
        Some(r#"{ "new_password": "rotated-pass-12" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/users/alice/password",
        Some(r#"{ "new_password": "short" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// 票 B2b-T4 AC：自改密码需验当前密码，验错拒绝；改后新旧密码交替生效。
#[tokio::test]
async fn self_change_password_requires_current_password() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_user(&app, &admin, "alice", USER_PASSWORD).await;
    let alice = login(&app, "alice", USER_PASSWORD).await;

    // 当前密码错：403 拒绝，密码不变。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/password",
        Some(
            r#"{ "current_password": "wrong-password-1", "new_password": "rotated-pass-12" }"#
                .to_string(),
        ),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "验错当前密码应拒绝");
    let resp = login_raw(&app, "alice", USER_PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::OK, "原密码应仍有效");

    // 正确：204；旧密码失效、新密码可登录；当前会话不被踢。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/password",
        Some(format!(
            r#"{{ "current_password": "{USER_PASSWORD}", "new_password": "rotated-pass-12" }}"#
        )),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = req_with_cookie(&app, "GET", "/api/v1/auth/me", None, Some(&alice)).await;
    assert_eq!(resp.status(), StatusCode::OK, "改密不踢当前会话");
    let resp = login_raw(&app, "alice", USER_PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "旧密码应失效");
    let resp = login_raw(&app, "alice", "rotated-pass-12").await;
    assert_eq!(resp.status(), StatusCode::OK, "新密码应可登录");

    // 新密码太短：422（与当前密码对错无关，形态校验先行）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/password",
        Some(r#"{ "current_password": "whatever-123", "new_password": "short" }"#.into()),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// 票 B2b-T4 AC：注册开关默认关（403）；开启后自注册成功且为非管理员、
/// 可登录；空库时即便开关开也 403（首个账号必须走 setup wizard）。
#[tokio::test]
async fn register_gated_by_config_switch() {
    // 默认形态（config 内置默认关，TestApp 装配与之一致）：403。
    let app = test_app().await;
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "eve", "password": "eve-password-12" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "开关关：register 403");

    // 开关开（装配等价 config 任一层打开）：空库先拒（setup 引导优先）。
    let app = test_app_with(true).await;
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "eve", "password": "eve-password-12" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "空库：register 403（先 setup）");

    // setup 建管理员后：自注册成功且为非管理员，可登录。
    common::setup_and_login(&app).await;
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "eve", "password": "eve-password-12" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "开关开：自注册 201");
    let body = body_json(resp).await;
    assert_eq!(body["username"], "eve");
    assert_eq!(body["is_admin"], false, "自注册必为非管理员");
    let resp = login_raw(&app, "eve", "eve-password-12").await;
    assert_eq!(resp.status(), StatusCode::OK, "注册后可登录");

    // 重名：409；短密码：422。
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "eve", "password": "eve-password-12" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "mallory", "password": "short" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// 票 B2b-T4 AC4 的字面贯通：经 **config.toml 文件层**打开开关（生产装配
/// 同一序列：toml → Config::load 合并 → AppState），register 即 201。
#[tokio::test]
async fn register_enabled_via_toml_file_reaches_router() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    std::fs::write(
        dir.path().join("config.toml"),
        "[auth]\nregistration_enabled = true\n",
    )
    .expect("写 config.toml");
    let cfg = sisyphus_server::config::Config::load(
        dir.path().to_path_buf(),
        sisyphus_server::config::Overrides::default(),
        sisyphus_server::config::Overrides::default(),
    )
    .expect("加载配置");
    assert!(cfg.registration_enabled, "toml 层应打开开关");

    let app = common::test_app_at_with(dir.path(), cfg.registration_enabled).await;
    common::setup_and_login(&app).await;
    let resp = common::post(
        &app,
        "/api/v1/auth/register",
        r#"{ "username": "toml-user", "password": "toml-user-pass-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "toml 打开后自注册应 201");
    assert_eq!(body_json(resp).await["is_admin"], false);
}

/// 建号/禁用端点的校验与错误形态：用户名字符集 422、缺 disabled 字段
/// 422、未知用户 404 统一 JSON。
#[tokio::test]
async fn user_management_validation_surface() {
    let app = test_app().await;
    let admin = common::setup_and_login(&app).await;

    // 用户名非法字符集（会破坏 /users/{name} 寻址）：422 定位 username。
    for bad in ["张三", "a/b", "a b"] {
        let resp = req_with_cookie(
            &app,
            "POST",
            "/api/v1/users",
            Some(format!(r#"{{ "username": "{bad}", "password": "{USER_PASSWORD}" }}"#)),
            Some(&admin),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad} 应 422");
        let body = body_json(resp).await;
        assert!(
            body["detail"]["errors"]
                .as_array()
                .is_some_and(|errors| errors.iter().any(|e| e["path"] == "username")),
            "应定位 username：{body}"
        );
    }

    // PATCH：缺 disabled 字段 422；未知用户 404；重复禁用幂等 200。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/users/admin",
        Some(r#"{ }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "缺字段应 422");
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/users/nobody",
        Some(r#"{ "disabled": true }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}

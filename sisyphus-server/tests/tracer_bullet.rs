//! Spec B2b tracer bullet 全链路（票 B2b-T7 AC，Router 缝）：空库首启 →
//! setup wizard 建全局 admin → 登录换 cookie → 建普通用户并分配项目角色 →
//! 三档矩阵（viewer/runner/admin 在定义保存端点上 403/403/200）→ PAT 创建
//! + Bearer 调用 → 跨源 POST 被 CSRF 拒 → 连续失败登录触发限流 → 禁用用户
//! 即刻踢线 → 项目 admin 写入机密（密文落库、永不可读）→ 审计页回放全程。
//!
//! 一条测试走完 B2b 的每一块服务端面，只断言 HTTP 状态码与 JSON 形态 +
//! store 缝直查（密文形态、审计行）。不起 socket、不 spawn 进程。

use axum::http::StatusCode;
use axum::response::Response;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie};

/// 测试用户共用密码。
const USER_PASSWORD: &str = "user-password-1";

/// 最小合法 Pipeline 定义（与 authorization.rs 同形）。
fn valid_definition() -> String {
    serde_json::json!({
        "name": "build",
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{ "type": "shell", "config": { "command": "cargo build" } }]
            }]
        }]
    })
    .to_string()
}

/// 直插一个非 admin 用户（返回用户 id；login 由调用侧按需进行）。
async fn insert_user(app: &TestApp, username: &str) -> i64 {
    let phc = sisyphus_server::auth::hash_password_blocking(USER_PASSWORD);
    let row = sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES (?, ?, 0, 0, 1, 1)",
    )
    .bind(username)
    .bind(&phc)
    .execute(&app.pool)
    .await
    .expect("直插用户");
    row.last_insert_rowid()
}

/// 用户名密码登录换会话 cookie。
async fn login_cookie(app: &TestApp, username: &str) -> Response {
    common::post(
        app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "{username}", "password": "{USER_PASSWORD}" }}"#),
    )
    .await
}

#[tokio::test]
async fn tracer_bullet_full_b2b_chain() {
    let app = common::test_app().await;

    // 1. 空库首启 → setup wizard 建全局 admin（user_created + 首登）。
    let setup = common::post(
        &app,
        "/api/v1/auth/setup",
        r#"{ "username": "admin", "password": "admin-password-1" }"#,
    )
    .await;
    assert_eq!(setup.status(), StatusCode::CREATED, "setup 建首个 admin");
    // 用户表非空后 setup 不可达（404）。
    let resp = common::post(
        &app,
        "/api/v1/auth/setup",
        r#"{ "username": "hacker", "password": "admin-password-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "有用户后 setup 404");

    // 2. 登录换 cookie。
    let login = common::post(
        &app,
        "/api/v1/auth/login",
        r#"{ "username": "admin", "password": "admin-password-1" }"#,
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK, "admin 登录");
    let admin = cookie_of(&login).expect("会话 cookie");

    // 3. 建普通用户并分配项目角色：dave（将被禁用）、alice/bob/carol 三档。
    insert_user(&app, "alice").await;
    insert_user(&app, "bob").await;
    insert_user(&app, "carol").await;
    insert_user(&app, "dave").await;
    // 建项目（全局 admin 专属）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "demo", "scm_type": "git", "scm_url": "https://example.com/demo" }"#
                .into(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "admin 建项目");
    // 分配三档角色（member_roles_changed）。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(
            r#"[ { "username": "alice", "role": "viewer" },
                 { "username": "bob", "role": "runner" },
                 { "username": "carol", "role": "admin" } ]"#
                .into(),
        ),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "分配成员");

    // 三档成员各自登录换 cookie。
    let alice = cookie_of(&login_cookie(&app, "alice").await).expect("alice cookie");
    let bob = cookie_of(&login_cookie(&app, "bob").await).expect("bob cookie");
    let carol = cookie_of(&login_cookie(&app, "carol").await).expect("carol cookie");

    // 4. 三档矩阵在定义保存端点上：viewer/runner 403、admin 200。
    for (cookie, role) in [(&alice, "viewer"), (&bob, "runner")] {
        let resp = req_with_cookie(
            &app,
            "PUT",
            "/api/v1/projects/demo/pipelines/build",
            Some(valid_definition()),
            Some(cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{role} PUT 应 403");
    }
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/pipelines/build",
        Some(valid_definition()),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "admin PUT 应 200");

    // 5. PAT 创建 + Bearer 调用业务端点（Bearer 免疫 CSRF，无需同源凭证）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "tracer" }"#.into()),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建 PAT");
    let token = body_json(resp).await["token"].as_str().expect("token").to_string();
    assert!(token.starts_with("sis_"), "PAT 族前缀");
    let resp = common::custom_req(
        &app,
        "GET",
        "/api/v1/projects",
        None,
        None,
        &[("authorization", format!("Bearer {token}"))],
        common::DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "Bearer 调 GET /projects");

    // 6. 跨源 POST 被 CSRF 拒（cookie 认证的非安全方法 + 异源 Origin）。
    let resp = common::custom_req(
        &app,
        "POST",
        "/api/v1/projects",
        Some(r#"{ "name": "evil", "scm_type": "git", "scm_url": "https://evil.example" }"#.into()),
        Some(&admin),
        &[("origin", "https://evil.example".to_string())],
        common::DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "跨源 POST 应 403");
    assert_eq!(body_json(resp).await["code"], "CSRF_REJECTED");

    // 7. 连续失败登录触发限流（独立 IP：不干扰主流程的 per-IP 键）。
    let attack_peer = std::net::SocketAddr::from(([10, 0, 0, 9], 52001));
    for _ in 0..5 {
        let resp = common::custom_req(
            &app,
            "POST",
            "/api/v1/auth/login",
            Some(r#"{ "username": "attacker", "password": "wrong-pass-1" }"#.into()),
            None,
            &[],
            attack_peer,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = common::custom_req(
        &app,
        "POST",
        "/api/v1/auth/login",
        Some(r#"{ "username": "attacker", "password": "wrong-pass-1" }"#.into()),
        None,
        &[],
        attack_peer,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS, "连续失败触发限流");

    // 8. 禁用用户即刻踢线：dave 登录拿 cookie → admin 禁用 → dave 请求 401。
    let dave = cookie_of(&login_cookie(&app, "dave").await).expect("dave cookie");
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&dave)).await;
    assert_eq!(resp.status(), StatusCode::OK, "禁用前 dave 可访问");
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/users/dave",
        Some(r#"{ "disabled": true }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "admin 禁用 dave");
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&dave)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "禁用后 dave 即刻 401");

    // 9. 项目 admin（carol）写入机密：密文落库（版本字节 + nonce + 密文）、
    //    值永不可读（GET 面只回名）。
    let plaintext = "tracer-secret-value";
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/secrets/TRACER_KEY",
        Some(serde_json::json!({ "value": plaintext }).to_string()),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "carol 写机密");
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&carol),
    )
    .await;
    let names: Vec<String> = body_json(resp)
        .await
        .as_array()
        .expect("清单")
        .iter()
        .map(|s| s["name"].as_str().expect("name").to_string())
        .collect();
    assert_eq!(names, ["TRACER_KEY"], "机密仅名清单");
    // store 缝直查：密文形态 + 明文不落库。
    let demo_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(&app.pool)
        .await
        .expect("demo 行");
    let ciphertext: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM secrets WHERE project_id = ? AND name = 'TRACER_KEY'")
            .bind(demo_id)
            .fetch_one(&app.pool)
            .await
            .expect("直查密文");
    assert_eq!(
        ciphertext[0],
        sisyphus_server::secrets::CIPHERTEXT_VERSION,
        "密文首字节为版本字节"
    );
    assert!(
        !String::from_utf8_lossy(&ciphertext).contains(plaintext),
        "密文不得含明文"
    );

    // 10. 审计页回放全程：事件类型逐类在列、actor 为实名、机密 detail 只记
    //     名（值不得出现）。
    let resp = req_with_cookie(&app, "GET", "/api/v1/audit", None, Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK, "全局 admin 查审计");
    let rows = body_json(resp).await;
    let rows = rows.as_array().expect("审计数组");
    let events: Vec<&str> = rows.iter().map(|r| r["event"].as_str().expect("event")).collect();
    for expected in [
        "user_created",    // setup 首个 admin + 直插用户（直插不落审计）……见下
        "login_success",   // admin/alice/bob/carol/dave 登录
        "project_created", // admin 建 demo
        "member_roles_changed",
        "pat_created",
        "secret_created",
        "user_disabled",  // admin 禁用 dave
        "login_failure",  // attacker 5 败 + 1 次限流触达
    ] {
        assert!(
            events.contains(&expected),
            "审计缺事件 {expected}：{events:?}"
        );
    }
    // 机密审计 detail 只记名；整个审计面无明文值。
    let secret_row = rows
        .iter()
        .find(|r| r["event"] == "secret_created")
        .expect("secret_created 行");
    assert_eq!(secret_row["detail"]["secret"], "TRACER_KEY");
    assert_eq!(secret_row["project"], "demo");
    let text = serde_json::to_string(&rows).expect("序列化");
    assert!(!text.contains(plaintext), "审计面不得出现机密明文");
    // 审计行数 = 上述事件笔数（appendix 不产生额外事件：定义保存不入审计）。
    assert!(
        rows.len() >= 8,
        "审计至少 8 类事件轨迹：{}",
        rows.len()
    );
    // AC：pipeline 保存不入审计（操作人已在业务表，避免双重记账，ADR-0015）
    // ——审计事件取值域即为契约，任何额外类型都是接线失误。
    const CONTRACT: &[&str] = &[
        "login_success",
        "login_failure",
        "logout",
        "user_created",
        "user_disabled",
        "user_enabled",
        "password_reset",
        "pat_created",
        "pat_revoked",
        "project_created",
        "member_roles_changed",
        "secret_created",
        "secret_overwritten",
        "secret_deleted",
    ];
    assert!(
        events.iter().all(|e| CONTRACT.contains(e)),
        "审计事件须全在契约内（pipeline 保存/构建不入审计）：{events:?}"
    );
}

//! Agent 注册面 Router 集成（票 #47 AC，Router 缝）：建条目/列表/详情/
//! 启停/编辑的授权与形态。全链路断言：token/注册码明文仅创建响应出现、
//! SHA-256 落库、停用即踢线（认证面/在线面即刻不命中）、磁盘占用入库
//! 可查。gRPC 通道认证/心跳会话在 `grpc_auth.rs`（proto 缝）。

use axum::http::StatusCode;

mod common;

use common::{TestApp, body_json, get, req, req_with_cookie, setup_and_login, test_app};

/// 全局 admin 建普通用户（返回其登录 cookie）。
async fn login_as_regular(app: &TestApp, admin_cookie: &str, username: &str) -> String {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/users",
        Some(format!(
            r#"{{"username": "{username}", "password": "alice-password-1"}}"#
        )),
        Some(admin_cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建普通用户");
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/auth/login",
        Some(format!(
            r#"{{ "username": "{username}", "password": "alice-password-1" }}"#
        )),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "普通用户登录");
    common::cookie_of(&resp).expect("登录下发 cookie")
}

/// 建 Agent 条目（返回响应体 JSON；断言 201 + 凭据形态）。
async fn create_agent(app: &TestApp, cookie: &str, body: &str) -> serde_json::Value {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/agents",
        Some(body.into()),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建条目应 201");
    body_json(resp).await
}

#[tokio::test]
async fn agents_require_global_admin() {
    let app = test_app().await;
    // 未认证：401（认证中间件全局面）。
    let resp = get(&app, "/api/v1/agents").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 普通用户（已认证非全局 admin）：403。
    let admin_cookie = setup_and_login(&app).await;
    let user_cookie = login_as_regular(&app, &admin_cookie, "alice").await;
    let _ = admin_cookie;
    for (method, path, body) in [
        ("GET", "/api/v1/agents", None),
        ("POST", "/api/v1/agents", Some(r#"{"name": "linux-1"}"#)),
        ("GET", "/api/v1/agents/linux-1", None),
        (
            "PATCH",
            "/api/v1/agents/linux-1",
            Some(r#"{"disabled": true}"#),
        ),
    ] {
        let resp = req_with_cookie(
            &app,
            method,
            path,
            body.map(String::from),
            Some(&user_cookie),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {path} 普通用户应 403"
        );
    }
    let _ = admin_cookie;
}

#[tokio::test]
async fn create_returns_token_and_register_code_plaintext_once() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    let body = create_agent(
        &app,
        &cookie,
        r#"{"name": "linux-1", "custom_labels": ["region=cn"], "max_concurrency": 2}"#,
    )
    .await;

    // token/注册码形态：sisa_ / sisa_reg_ 前缀 + 43 字符正文。
    let token = body["token"].as_str().expect("token");
    let register_code = body["register_code"].as_str().expect("register_code");
    assert!(token.starts_with("sisa_"), "{token}");
    assert_eq!(token.len(), "sisa_".len() + 43);
    assert!(register_code.starts_with("sisa_reg_"), "{register_code}");
    assert_eq!(register_code.len(), "sisa_reg_".len() + 43);

    // agent 视图：离线、未停用、系统标签空、自定义标签/槽位落定。
    let agent = &body["agent"];
    assert_eq!(agent["name"], "linux-1");
    assert_eq!(agent["online"], false);
    assert_eq!(agent["disabled"], false);
    assert_eq!(agent["system_labels"], serde_json::json!([]));
    assert_eq!(agent["custom_labels"], serde_json::json!(["region=cn"]));
    assert_eq!(agent["max_concurrency"], 2);
    assert_eq!(agent["active_jobs"], 0);
    assert!(agent["disk_usage"].is_null(), "从未上报无磁盘占用");

    // 明文只在创建响应出现：库里只存 SHA-256（直查 token_hash 列），
    // 且列表/详情永不再回显值。
    let token_hash: String =
        sqlx::query_scalar("SELECT token_hash FROM agents WHERE name = 'linux-1'")
            .fetch_one(&app.pool)
            .await
            .expect("直查 token_hash");
    assert_ne!(token_hash, token, "库中不是明文");
    assert_eq!(token_hash.len(), 64, "SHA-256 十六进制");
    let code_hash: String =
        sqlx::query_scalar("SELECT register_code_hash FROM agents WHERE name = 'linux-1'")
            .fetch_one(&app.pool)
            .await
            .expect("直查注册码哈希");
    assert_ne!(code_hash, register_code);
    assert!(
        !token_hash.contains(register_code),
        "注册码哈希独立列、不落 token_hash"
    );

    // 审计（ADR-0015：Agent 建立 + 注册码签发入账；detail 只记名）。
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, event_type FROM audit_log WHERE event_type = 'agent_created'",
    )
    .fetch_all(&app.pool)
    .await
    .expect("直查审计");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "admin", "操作人实名");
    let audit_body = body_json(get_with_cookie(&app, "/api/v1/audit", &cookie).await).await;
    assert!(
        !audit_body
            .as_array()
            .expect("审计清单数组")
            .iter()
            .any(|e| e["event"] == "agent_created" && e["detail"].to_string().contains(&token[..8])),
        "审计 detail 永不含 token 值"
    );

    let list = get_with_cookie(&app, "/api/v1/agents", &cookie).await;
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = body_json(list).await;
    let listed = &list_body[0];
    assert_eq!(listed["name"], "linux-1");
    assert!(listed.get("token").is_none(), "列表永不含值");
    assert!(listed.get("register_code").is_none(), "列表永不含注册码");
}

#[tokio::test]
async fn create_validates_name_labels_and_concurrency() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    for (body, issue_path) in [
        (r#"{"name": "   "}"#, "name"),
        (r#"{"name": "a/b"}"#, "name"),
        (
            r#"{"name": "linux-1", "max_concurrency": 0}"#,
            "max_concurrency",
        ),
        (
            r#"{"name": "linux-1", "custom_labels": ["region"]}"#,
            "custom_labels",
        ),
        (
            r#"{"name": "linux-1", "custom_labels": ["=cn"]}"#,
            "custom_labels",
        ),
    ] {
        let resp = req_with_cookie(
            &app,
            "POST",
            "/api/v1/agents",
            Some(body.into()),
            Some(&cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let json = body_json(resp).await;
        assert_eq!(json["code"], "VALIDATION_FAILED");
        let errors = json["detail"]["errors"].as_array().expect("错误清单");
        assert!(
            errors.iter().any(|e| e["path"] == issue_path),
            "{body} 应定位到 {issue_path}：{errors:?}"
        );
    }

    // 重名：409。
    create_agent(&app, &cookie, r#"{"name": "linux-1"}"#).await;
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/agents",
        Some(r#"{"name": "linux-1"}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn list_and_detail_show_slots_and_disk_usage() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_agent(&app, &cookie, r#"{"name": "linux-1"}"#).await;

    // 列表（按名排序、单条）。
    let resp = get_with_cookie(&app, "/api/v1/agents", &cookie).await;
    let list = body_json(resp).await;
    assert_eq!(list.as_array().expect("数组").len(), 1);
    assert_eq!(list[0]["name"], "linux-1");
    assert_eq!(list[0]["active_jobs"], 0);

    // 详情与列表同形。
    let resp = get_with_cookie(&app, "/api/v1/agents/linux-1", &cookie).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = body_json(resp).await;
    assert_eq!(detail["name"], "linux-1");
    assert_eq!(detail["active_jobs"], 0);

    // 详情 404（不存在的 Agent）。
    let resp = get_with_cookie(&app, "/api/v1/agents/nope", &cookie).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");
}

#[tokio::test]
async fn patch_disables_kicks_and_edits_spec() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_agent(
        &app,
        &cookie,
        r#"{"name": "linux-1", "max_concurrency": 1}"#,
    )
    .await;

    // 改槽位 + 自定义标签（整组替换）。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/linux-1",
        Some(r#"{"max_concurrency": 3, "custom_labels": ["region=eu", "gpu=nvidia"]}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let updated = body_json(resp).await;
    assert_eq!(updated["max_concurrency"], 3);
    assert_eq!(
        updated["custom_labels"],
        serde_json::json!(["region=eu", "gpu=nvidia"])
    );

    // 停用即踢线：认证面不命中（同 token 已无法换到连接）——
    // gRPC 面 find_active_by_hash 语义经 store 断言（全链路在 grpc_auth.rs）。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/linux-1",
        Some(r#"{"disabled": true}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let disabled = body_json(resp).await;
    assert_eq!(disabled["disabled"], true);
    assert_eq!(
        disabled["online"], false,
        "停用不改变在线标记（踢线由认证面承载）"
    );

    // 审计（ADR-0015：停用即吊销 token 入账）。
    let disabled_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_type = 'agent_disabled'")
            .fetch_one(&app.pool)
            .await
            .expect("直查审计");
    assert_eq!(disabled_rows, 1);

    // 启用恢复。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/linux-1",
        Some(r#"{"disabled": false}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(body_json(resp).await["disabled"], false);
    let enabled_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_type = 'agent_enabled'")
            .fetch_one(&app.pool)
            .await
            .expect("直查审计");
    assert_eq!(enabled_rows, 1);

    // 编辑校验：槽位 0 422；不存在的 Agent 404。
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/linux-1",
        Some(r#"{"max_concurrency": 0}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/nope",
        Some(r#"{"disabled": true}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn register_code_exchanges_for_token_one_time() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    // 建条目拿到注册码与 token。
    let created = create_agent(&app, &cookie, r#"{"name": "linux-1"}"#).await;
    let register_code = created["register_code"].as_str().expect("注册码");
    let _original_token = created["token"].as_str().expect("原 token");

    // 兑码：公开端点（无 cookie）返回 per-Agent token（sisa_ + 43）。
    let body = format!(r#"{{"name": "linux-1", "register_code": "{register_code}"}}"#);
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "有效注册码应兑码成功");
    let json = body_json(resp).await;
    let token = json["token"].as_str().expect("token");
    assert!(token.starts_with("sisa_"), "{token}");
    assert_eq!(token.len(), "sisa_".len() + 43);

    // 兑码即换新：库里 token_hash 已轮换（旧 token 吊销、新 token 可用）。
    let token_hash: String =
        sqlx::query_scalar("SELECT token_hash FROM agents WHERE name = 'linux-1'")
            .fetch_one(&app.pool)
            .await
            .expect("直查 token_hash");
    assert_ne!(token_hash, token, "库里仍是哈希");
    assert_eq!(token_hash.len(), 64);
    assert_ne!(
        token_hash,
        sisyphus_server::auth::token_hash(_original_token),
        "兑码吊销旧 token（换新）"
    );

    // 注册码置已用：重兑 409（一次性）。
    let body = format!(r#"{{"name": "linux-1", "register_code": "{register_code}"}}"#);
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(body)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "重兑应 409");
    assert_eq!(body_json(resp).await["code"], "CONFLICT");

    // 审计（ADR-0015：注册码兑 token 入账；actor 记 Agent 名）。
    let events: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT actor, event_type, detail FROM audit_log WHERE event_type = 'agent_registered'",
    )
    .fetch_all(&app.pool)
    .await
    .expect("直查审计");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "linux-1", "actor 记 Agent 名");
    assert!(events[0].2.as_deref().unwrap_or("").contains("linux-1"));
}

#[tokio::test]
async fn register_rejects_invalid_used_expired_and_disabled() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let created = create_agent(&app, &cookie, r#"{"name": "linux-1"}"#).await;
    let register_code = created["register_code"].as_str().expect("注册码");

    // 无效注册码（哈希无匹配）：404。
    let body = r#"{"name": "linux-1", "register_code": "sisa_reg_deadbeef"}"#;
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(body.into())).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无效注册码应 404");
    assert_eq!(body_json(resp).await["code"], "NOT_FOUND");

    // 已用（先正常兑一次）：409。
    let ok_body = format!(r#"{{"name": "linux-1", "register_code": "{register_code}"}}"#);
    let resp = req(
        &app,
        "POST",
        "/api/v1/agent/register",
        Some(ok_body.clone()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "先兑一次");
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(ok_body)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "已用应 409");

    // 停用：403（停用即踢线，不配发 token）。
    let created2 = create_agent(&app, &cookie, r#"{"name": "linux-2"}"#).await;
    let code2 = created2["register_code"].as_str().expect("注册码");
    let resp = req_with_cookie(
        &app,
        "PATCH",
        "/api/v1/agents/linux-2",
        Some(r#"{"disabled": true}"#.into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "停用 linux-2");
    let body = format!(r#"{{"name": "linux-2", "register_code": "{code2}"}}"#);
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(body)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "停用应 403");
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");

    // 过期：403（直接改库把有效期拨到过去——repo 缝已验过期判据）。
    let created3 = create_agent(&app, &cookie, r#"{"name": "linux-3"}"#).await;
    let code3 = created3["register_code"].as_str().expect("注册码");
    sqlx::query("UPDATE agents SET register_code_expires_at = 1 WHERE name = 'linux-3'")
        .execute(&app.pool)
        .await
        .expect("拨过期");
    let body = format!(r#"{{"name": "linux-3", "register_code": "{code3}"}}"#);
    let resp = req(&app, "POST", "/api/v1/agent/register", Some(body)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "过期应 403");

    // 空 name / 空注册码：422。
    for body in [
        r#"{"name": "", "register_code": "sisa_reg_x"}"#,
        r#"{"name": "linux-1", "register_code": ""}"#,
        "not-json",
    ] {
        let resp = req(&app, "POST", "/api/v1/agent/register", Some(body.into())).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
}

/// 带 cookie 的 GET（走同源 CSRF 头形态，与 common 的 req_with_cookie 同构）。
async fn get_with_cookie(app: &TestApp, path: &str, cookie: &str) -> axum::response::Response {
    req_with_cookie(app, "GET", path, None, Some(cookie)).await
}

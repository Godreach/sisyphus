//! 触发器 REST 端点进程内集成（票 B2c-T6，Spec B2c Router 缝）：cron/poll
//! 触发器 CRUD 的可观察行为——项目 admin 档列/建/改配置与启停、runner 403、
//! 无角色 404、spec 校验、poll 启用重置基线。只断言 HTTP 状态码与 JSON 形态，
//! 不起 socket、不 spawn 进程。
//!
//! 触发源本身（cron 扫表 + poll 轮询）的基线/去重/节奏/失败历史逻辑由
//! `trigger::tests` 内联单测覆盖（假探测 + 假时钟）；本缝只验 REST 配置面。

use axum::http::StatusCode;
use axum::response::Response;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie};

/// 直插一个非 admin 用户并 login 换会话 cookie（与 authorization 缝同款）。
async fn user_cookie(app: &TestApp, username: &str) -> String {
    let phc = sisyphus_server::auth::hash_password_blocking("user-password-1");
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES (?, ?, 0, 0, 1, 1)",
    )
    .bind(username)
    .bind(&phc)
    .execute(&app.pool)
    .await
    .expect("直插用户");
    let resp = common::post(
        app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "{username}", "password": "user-password-1" }}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login {username}");
    cookie_of(&resp).expect("会话 cookie")
}

/// 装配 + 全局 admin + 项目 demo + pipeline release + 三档成员。
/// 返回 (app, admin, alice=viewer, bob=runner, carol=admin, dave=无角色)。
async fn fixture() -> (TestApp, String, String, String, String, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin, "demo").await;
    save_definition(&app, &admin, "release").await;
    let (alice, bob, carol, dave) = (
        user_cookie(&app, "alice").await,
        user_cookie(&app, "bob").await,
        user_cookie(&app, "carol").await,
        user_cookie(&app, "dave").await,
    );
    assign_members(
        &app,
        &admin,
        "demo",
        r#"[ { "username": "alice", "role": "viewer" },
             { "username": "bob", "role": "runner" },
             { "username": "carol", "role": "admin" } ]"#,
    )
    .await;
    (app, admin, alice, bob, carol, dave)
}

async fn create_project(app: &TestApp, cookie: &str, name: &str) {
    let resp = req_with_cookie(
        app,
        "POST",
        "/api/v1/projects",
        Some(format!(
            r#"{{ "name": "{name}", "scm_type": "git", "scm_url": "https://example.com/{name}" }}"#
        )),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "admin 建项目 {name}");
}

/// 存 release pipeline 定义（项目 admin 档；全局 admin 隐含 admin）。
async fn save_definition(app: &TestApp, cookie: &str, pipeline: &str) {
    let body = serde_json::json!({
        "name": "build",
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{ "type": "shell", "config": { "command": "cargo build" } }]
            }]
        }]
    })
    .to_string();
    let resp = req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}"),
        Some(body),
        Some(cookie),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "存定义应 200");
}

async fn assign_members(app: &TestApp, admin: &str, project: &str, body: &str) {
    let resp = req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/{project}/members"),
        Some(body.into()),
        Some(admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "分配成员");
}

// ----- 端点请求辅助 -----

async fn list_triggers(app: &TestApp, cookie: &str, pipeline: &str) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/triggers"),
        None,
        Some(cookie),
    )
    .await
}

async fn create_trigger(app: &TestApp, cookie: &str, pipeline: &str, body: &str) -> Response {
    req_with_cookie(
        app,
        "POST",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/triggers"),
        Some(body.into()),
        Some(cookie),
    )
    .await
}

async fn get_trigger(app: &TestApp, cookie: &str, pipeline: &str, kind: &str) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/triggers/{kind}"),
        None,
        Some(cookie),
    )
    .await
}

async fn patch_trigger(app: &TestApp, cookie: &str, pipeline: &str, kind: &str, body: &str) -> Response {
    req_with_cookie(
        app,
        "PATCH",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/triggers/{kind}"),
        Some(body.into()),
        Some(cookie),
    )
    .await
}

const CRON_BODY: &str = r#"{ "kind": "cron", "cron": { "expr": "0 2 * * *" } }"#;
const POLL_BODY: &str = r#"{ "kind": "poll", "poll": { "interval_minutes": 5 } }"#;

// ----- 用例 -----

/// AC：项目 admin 列/建/改触发器；同 (pipeline, kind) 重复 409。
#[tokio::test]
async fn admin_can_create_list_get_and_patch_triggers() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;

    // 建 cron + poll（各一）。
    let resp = create_trigger(&app, &admin, "release", CRON_BODY).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建 cron");
    let cron = body_json(resp).await;
    assert_eq!(cron["kind"], "cron");
    assert_eq!(cron["spec"]["expr"], "0 2 * * *");
    assert!(cron["enabled"].as_bool().unwrap(), "缺省启用");
    assert!(cron["baseline_commit"].is_null(), "cron 无基线");

    let resp = create_trigger(&app, &admin, "release", POLL_BODY).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "建 poll");
    let poll = body_json(resp).await;
    assert_eq!(poll["kind"], "poll");
    assert_eq!(poll["spec"]["interval_minutes"], 5);
    assert!(poll["baseline_commit"].is_null(), "poll 基线待首探");
    assert!(poll["last_probe_at"].is_null());

    // 同 (pipeline, kind) 重复 → 409。
    let resp = create_trigger(&app, &admin, "release", CRON_BODY).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "同类重复 409");

    // 列表：两类（按 kind 序）。
    let resp = list_triggers(&app, &admin, "release").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let arr = list.as_array().expect("数组");
    assert_eq!(arr.len(), 2, "cron + poll 各一");
    assert_eq!(arr[0]["kind"], "cron", "按 kind 序");
    assert_eq!(arr[1]["kind"], "poll");

    // 详情：取 cron / poll / 未知 kind。
    assert_eq!(
        get_trigger(&app, &admin, "release", "cron").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get_trigger(&app, &admin, "release", "poll").await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get_trigger(&app, &admin, "release", "bogus").await.status(),
        StatusCode::NOT_FOUND,
        "未知 kind 404"
    );

    // 改 cron spec + 停用。
    let resp = patch_trigger(
        &app,
        &admin,
        "release",
        "cron",
        r#"{ "cron": { "expr": "0 6 * * 1-5" }, "enabled": false }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "改 cron");
    let cron = body_json(resp).await;
    assert_eq!(cron["spec"]["expr"], "0 6 * * 1-5");
    assert_eq!(cron["enabled"], false);

    // 改 poll 节奏。
    let resp = patch_trigger(
        &app,
        &admin,
        "release",
        "poll",
        r#"{ "poll": { "interval_minutes": 15 } }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "改 poll");
    let poll = body_json(resp).await;
    assert_eq!(poll["spec"]["interval_minutes"], 15);
}

/// AC：poll 启用（false→true）重置基线（ADR-0016「启用时记基线不触发」）。
#[tokio::test]
async fn poll_enable_resets_baseline() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;
    create_trigger(&app, &admin, "release", POLL_BODY).await;
    // 停用 poll（无触发引擎在跑，基线仍空）。
    patch_trigger(&app, &admin, "release", "poll", r#"{ "enabled": false }"#).await;
    // 直插基线（模拟禁用前曾探测记过基线）。
    sqlx::query("UPDATE triggers SET baseline_commit = 'abc123' WHERE kind = 'poll'")
        .execute(&app.pool)
        .await
        .expect("直插基线");
    // 启用 → reset_baseline 清基线。
    let resp = patch_trigger(&app, &admin, "release", "poll", r#"{ "enabled": true }"#).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let poll = body_json(resp).await;
    assert_eq!(poll["enabled"], true);
    assert!(poll["baseline_commit"].is_null(), "启用重置基线（下次探测重记）");
}

/// AC：runner 档不足（触发器管理需项目 admin 档）→ 403。
#[tokio::test]
async fn runner_is_forbidden() {
    let (app, _admin, _alice, bob, _carol, _dave) = fixture().await;
    // 先由 admin 建 cron，runner 再尝试各类操作均 403。
    create_trigger(&app, &_admin, "release", CRON_BODY).await;
    assert_eq!(
        list_triggers(&app, &bob, "release").await.status(),
        StatusCode::FORBIDDEN,
        "runner 列触发器 403"
    );
    assert_eq!(
        create_trigger(&app, &bob, "release", CRON_BODY)
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "runner 建触发器 403"
    );
    assert_eq!(
        get_trigger(&app, &bob, "release", "cron").await.status(),
        StatusCode::FORBIDDEN,
        "runner 取触发器 403"
    );
    assert_eq!(
        patch_trigger(&app, &bob, "release", "cron", r#"{ "enabled": false }"#)
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "runner 改触发器 403"
    );
}

/// AC：viewer 档不足 → 403。
#[tokio::test]
async fn viewer_is_forbidden() {
    let (app, _admin, alice, _bob, _carol, _dave) = fixture().await;
    assert_eq!(
        list_triggers(&app, &alice, "release").await.status(),
        StatusCode::FORBIDDEN,
        "viewer 列触发器 403"
    );
    assert_eq!(
        create_trigger(&app, &alice, "release", CRON_BODY)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

/// AC：无角色项目对触发器端点 404 不可见（存在性不外泄，ADR-0014）。
#[tokio::test]
async fn no_role_is_404_invisible() {
    let (app, _admin, _alice, _bob, _carol, dave) = fixture().await;
    assert_eq!(
        list_triggers(&app, &dave, "release").await.status(),
        StatusCode::NOT_FOUND,
        "无角色 404（与不存在同形）"
    );
    assert_eq!(
        create_trigger(&app, &dave, "release", CRON_BODY)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_trigger(&app, &dave, "release", "cron").await.status(),
        StatusCode::NOT_FOUND
    );
}

/// AC：建触发器要求 pipeline 存在——不存在 404（不建孤儿触发器）。
#[tokio::test]
async fn create_on_missing_pipeline_is_404() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;
    let resp = create_trigger(&app, &admin, "nope", CRON_BODY).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "pipeline 不存在 404");
}

/// AC：spec 校验——坏 cron（非 5 字段）/坏 poll（节奏 0）/patch spec 与
/// kind 不匹配 → 422。
#[tokio::test]
async fn invalid_spec_is_422() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;
    // 坏 cron：4 字段。
    let resp = create_trigger(
        &app,
        &admin,
        "release",
        r#"{ "kind": "cron", "cron": { "expr": "0 2 * *" } }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "坏 cron 422");
    // 坏 poll：节奏 0。
    let resp = create_trigger(
        &app,
        &admin,
        "release",
        r#"{ "kind": "poll", "poll": { "interval_minutes": 0 } }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "坏 poll 422");
    // cron 缺 spec → 422。
    let resp = create_trigger(&app, &admin, "release", r#"{ "kind": "cron" }"#).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "cron 缺 spec 422");

    // 建合法 cron 后：patch 给 poll spec（与 kind 不匹配）→ 422。
    create_trigger(&app, &admin, "release", CRON_BODY).await;
    let resp = patch_trigger(
        &app,
        &admin,
        "release",
        "cron",
        r#"{ "poll": { "interval_minutes": 5 } }"#,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "spec 与 kind 不匹配 422"
    );
}

/// AC：poll interval 缺省取 config 默认（5 分钟）。
#[tokio::test]
async fn poll_interval_defaults_to_config_when_omitted() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;
    // poll spec 不给 interval → 取 config 默认 5。
    let resp = create_trigger(
        &app,
        &admin,
        "release",
        r#"{ "kind": "poll", "poll": {} }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let poll = body_json(resp).await;
    assert_eq!(poll["spec"]["interval_minutes"], 5, "缺省取 config 默认");
}

/// AC：触发器端点删除本批不做——DELETE 不注册 → 405（路径命中、方法未注册）。
#[tokio::test]
async fn delete_endpoint_not_registered_is_405() {
    let (app, admin, _alice, _bob, _carol, _dave) = fixture().await;
    create_trigger(&app, &admin, "release", CRON_BODY).await;
    let resp = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/projects/demo/pipelines/release/triggers/cron",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "DELETE 本批不做 → 405"
    );
}

/// 未认证 → 401（全局中间件面，与各受保护端点同纪律）。
#[tokio::test]
async fn unauthenticated_is_401() {
    let app = common::test_app().await;
    let resp = list_triggers(&app, "", "release").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

//! 构建 REST 端点进程内集成（票 B2c-T5 AC）：触发 / 取消 / 重跑 / 列表 /
//! 详情 + runner 档授权矩阵（viewer 403 / runner ok / 无角色 404）+ 缺机密名
//! 任务失败记名不泄值。
//!
//! 消费 common harness（无 sched 循环）：缺机密失败 / from_failed 重跑前置
//! 失败态等经 [`common::drive_build`] 手动驱动 engine（与 sched 共享同一
//! engine，drive 幂等）。全链路真实下发闭环（proto 缝 fake Agent）在
//! `b2c_tracer_bullet.rs`。只断言 HTTP 状态码 + JSON 形态 + store 缝直查。

use axum::http::StatusCode;
use axum::response::Response;

mod common;

use common::{TestApp, body_json, body_text, cookie_of, drive_build, req_with_cookie};
use sisyphus_server::engine::TriggerDetail;
use sisyphus_server::store::builds::{BuildRepo, BuildStatus};
use sisyphus_server::store::jobs::JobRepo;

/// 测试用户共用密码（与 authorization.rs 同形）。
const USER_PASSWORD: &str = "user-password-1";

/// 直插一个非 admin 用户并 login 换会话 cookie。
async fn user_cookie(app: &TestApp, username: &str) -> String {
    let phc = sisyphus_server::auth::hash_password_blocking(USER_PASSWORD);
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
        &format!(r#"{{ "username": "{username}", "password": "{USER_PASSWORD}" }}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login {username}");
    cookie_of(&resp).expect("会话 cookie")
}

/// 装配 + 全局 admin + 项目 + 三档成员（alice=viewer/bob=runner/carol=admin/
/// dave=无角色）。返回各 cookie。
async fn fixture() -> (TestApp, String, String, String, String, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin, "demo").await;
    let (alice, bob, carol, dave) = (
        user_cookie(&app, "alice").await,
        user_cookie(&app, "bob").await,
        user_cookie(&app, "carol").await,
        user_cookie(&app, "dave").await,
    );
    assign_members(&app, &admin).await;
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

/// 全局 admin 整组分配三档角色。
async fn assign_members(app: &TestApp, admin: &str) {
    let resp = req_with_cookie(
        app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(
            r#"[ { "username": "alice", "role": "viewer" },
                 { "username": "bob", "role": "runner" },
                 { "username": "carol", "role": "admin" } ]"#
                .into(),
        ),
        Some(admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "分配成员");
}

/// 存 pipeline 定义（项目 admin 档，carol）。
async fn save_definition(app: &TestApp, cookie: &str, pipeline: &str, body: String) {
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

/// 最小合法定义（无参数/机密）。
fn minimal_definition() -> String {
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

/// 带一个 string 参数（默认 x86_64）的定义——验参数覆盖。
fn definition_with_param() -> String {
    serde_json::json!({
        "name": "build",
        "parameters": [{
            "name": "target",
            "type": "string",
            "required": true,
            "default": "x86_64"
        }],
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{ "type": "shell", "config": { "command": "echo ${target}" } }]
            }]
        }]
    })
    .to_string()
}

/// 任务引用机密名的定义（验缺机密名失败记名）。
fn definition_with_secret(secret: &str) -> String {
    serde_json::json!({
        "name": "build",
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "secrets": [secret],
                "steps": [{ "type": "shell", "config": { "command": "echo hi" } }]
            }]
        }]
    })
    .to_string()
}

// ----- 端点请求辅助 -----

async fn trigger(app: &TestApp, cookie: &str, pipeline: &str, body: &str) -> Response {
    req_with_cookie(
        app,
        "POST",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds"),
        Some(body.into()),
        Some(cookie),
    )
    .await
}

async fn list_builds(app: &TestApp, cookie: &str, pipeline: &str, query: &str) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds{query}"),
        None,
        Some(cookie),
    )
    .await
}

async fn detail(app: &TestApp, cookie: &str, pipeline: &str, number: i64) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds/{number}"),
        None,
        Some(cookie),
    )
    .await
}

async fn cancel(app: &TestApp, cookie: &str, pipeline: &str, number: i64) -> Response {
    req_with_cookie(
        app,
        "POST",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds/{number}/cancel"),
        None,
        Some(cookie),
    )
    .await
}

async fn rerun(app: &TestApp, cookie: &str, pipeline: &str, number: i64, body: &str) -> Response {
    req_with_cookie(
        app,
        "POST",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds/{number}/rerun"),
        Some(body.into()),
        Some(cookie),
    )
    .await
}

/// 从触发/重跑响应取 build_id（DB 缝直改用）。
fn build_id_of(body: &serde_json::Value) -> i64 {
    body["build_id"].as_i64().expect("build_id")
}

/// 触发并返回 (build_id, number)。
async fn trigger_build(app: &TestApp, cookie: &str, pipeline: &str, body: &str) -> (i64, i64) {
    let resp = trigger(app, cookie, pipeline, body).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "触发应 202");
    let body = body_json(resp).await;
    (build_id_of(&body), body["number"].as_i64().expect("number"))
}

// ===========================================================================
// 触发 + 详情
// ===========================================================================

/// AC：手动触发 runner 202 + 构建号；viewer 403；无角色 404；参数覆盖与分支
/// 指定生效（默认值语义）；pipeline 不存在 404；bad body 422。
#[tokio::test]
async fn trigger_runner_viewer_norole_and_param_override() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", definition_with_param()).await;

    // runner 触发：202 + 构建号；参数覆盖 + 分支/commit 指定。
    let resp = trigger(
        &app,
        &bob,
        "build",
        r#"{ "params": { "target": "aarch64" }, "branch": "dev", "commit": "deadbeef" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "runner 触发 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1);
    assert_eq!(body["attempt"], 1);
    assert_eq!(body["status"], "queued");
    let build_id = build_id_of(&body);

    // store 缝直查：触发上下文含覆盖参数、分支与 commit（默认值语义——手动覆盖叠加）。
    let row = BuildRepo::new(app.pool.clone())
        .get(build_id)
        .await
        .expect("查")
        .expect("应存在");
    let trig: TriggerDetail = serde_json::from_str(&row.trigger_detail).expect("触发上下文可解析");
    assert_eq!(trig.by, "bob", "触发人为认证用户实名");
    assert_eq!(trig.branch.as_deref(), Some("dev"), "分支指定生效");
    assert_eq!(trig.commit.as_deref(), Some("deadbeef"), "commit 指定生效");
    assert_eq!(
        trig.params
            .iter()
            .find(|p| p.name == "target")
            .map(|p| p.value.as_str()),
        Some("aarch64"),
        "参数覆盖落库"
    );

    // 详情：状态/触发人/attempt/阶段任务可见。
    let resp = detail(&app, &bob, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::OK, "runner 详情 200");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1);
    assert_eq!(body["status"], "queued");
    assert_eq!(body["trigger"], "manual");
    assert_eq!(body["trigger_by"], "bob");
    assert_eq!(body["attempt"], 1);
    assert_eq!(body["elapsed_ms"], serde_json::Value::Null, "未运行无耗时");
    assert_eq!(body["stages"].as_array().expect("stages").len(), 1);
    // 未驱动的构建：阶段已下发无任务行（queued 构建尚未 drive）。
    assert_eq!(body["stages"][0]["name"], "build");

    // viewer 触发 → 403；无角色 → 404。
    let resp = trigger(&app, &alice, "build", "{}").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 触发 403");
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
    let resp = trigger(&app, &dave, "build", "{}").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色触发 404");

    // 空体触发等价全默认（无覆盖、缺省分支）。
    let resp = trigger(&app, &bob, "build", "").await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "空体触发 202");

    // pipeline 不存在 → 404（runner 已是项目成员，第二层 404）。
    let resp = trigger(&app, &bob, "nope", "{}").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "pipeline 不存在 404");

    // bad body → 422。
    let resp = trigger(&app, &bob, "build", "{not json}").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad body 422"
    );
}

// ===========================================================================
// 取消
// ===========================================================================

/// AC：取消 build 级（排队中移出）；终态幂等；runner 202 / viewer 403 /
/// 无角色 404；不存在 404。
#[tokio::test]
async fn cancel_removes_queued_is_idempotent_and_matrix() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", minimal_definition()).await;
    let (build_id, _number) = trigger_build(&app, &bob, "build", "{}").await;

    // viewer / 无角色：403 / 404（先于任何状态迁移）。
    let resp = cancel(&app, &alice, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 取消 403");
    let resp = cancel(&app, &dave, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色取消 404");

    // runner 取消：202，构建 cancelled。
    let resp = cancel(&app, &bob, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "runner 取消 202");
    assert_eq!(body_json(resp).await["status"], "cancelled");
    let row = BuildRepo::new(app.pool.clone())
        .get(build_id)
        .await
        .expect("查")
        .expect("应存在");
    assert_eq!(row.status, BuildStatus::Cancelled);
    assert!(row.cancelled_at.is_some(), "记取消时刻");

    // 终态幂等：再取消 202、状态不变。
    let resp = cancel(&app, &bob, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "终态取消幂等 202");
    assert_eq!(body_json(resp).await["status"], "cancelled");

    // 不存在构建号 → 404。
    let resp = cancel(&app, &bob, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "不存在 404");
}

// ===========================================================================
// 重跑
// ===========================================================================

/// AC：重跑两模式——from_scratch 新号 attempt=1（by=当前用户）/ from_failed
/// 同号 attempt+1；from_failed 要求 failed/cancelled/timeout 终态（否则 409）；
/// runner 202 / viewer 403 / 无角色 404。
#[tokio::test]
async fn rerun_two_modes_and_matrix() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", minimal_definition()).await;

    // from_scratch：触发 #1（bob）→ carol 从头重跑 → 新号 #2、attempt=1、
    // by=carol（重跑人，非原触发人）。
    let (_b1, _n1) = trigger_build(&app, &bob, "build", r#"{ "branch": "main" }"#).await;
    let resp = rerun(&app, &carol, "build", 1, r#"{ "mode": "from_scratch" }"#).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "from_scratch 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 2, "新号");
    assert_eq!(body["attempt"], 1, "attempt=1");
    let b2 = build_id_of(&body);
    let row = BuildRepo::new(app.pool.clone())
        .get(b2)
        .await
        .expect("查")
        .expect("应存在");
    let trig: TriggerDetail = serde_json::from_str(&row.trigger_detail).expect("触发上下文");
    assert_eq!(trig.by, "carol", "from_scratch 的 by 为重跑人");
    assert_eq!(trig.branch.as_deref(), Some("main"), "复制原触发上下文");

    // from_failed 要求终态：build #1 仍 queued → 409。
    let resp = rerun(&app, &bob, "build", 1, r#"{ "mode": "from_failed" }"#).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "queued 不可 from_failed 409"
    );
    assert_eq!(body_json(resp).await["code"], "CONFLICT");

    // from_failed：独立 sec 管线（引用缺失机密）触发 #1 → drive 组装缺机密
    // 失败 → from_failed 同号 attempt+1。独立管线避免与 build 的 FIFO 排队
    // 互相干扰（drive 提升的是该 pipeline 号最小的排队者）。
    save_definition(&app, &carol, "sec", definition_with_secret("MISSING_KEY")).await;
    let (b_sec, _) = trigger_build(&app, &bob, "sec", "{}").await;
    drive_build(&app, b_sec).await; // 组装缺机密 → 任务 failed + 级联 → 构建 failed
    let row = BuildRepo::new(app.pool.clone())
        .get(b_sec)
        .await
        .expect("查")
        .expect("应存在");
    assert_eq!(row.status, BuildStatus::Failed, "缺机密构建失败");

    let resp = rerun(&app, &bob, "sec", 1, r#"{ "mode": "from_failed" }"#).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "from_failed 202");
    let body = body_json(resp).await;
    assert_eq!(body["number"], 1, "同号延续");
    assert_eq!(body["attempt"], 2, "attempt+1");
    // 重跑后 drive 重新组装仍缺机密 → 仍 failed（attempt=2）。
    let row = BuildRepo::new(app.pool.clone())
        .get(b_sec)
        .await
        .expect("查")
        .expect("应存在");
    assert_eq!(
        row.status,
        BuildStatus::Failed,
        "重跑续跑至失败（机密仍缺）"
    );
    assert_eq!(row.attempt, 2);

    // viewer / 无角色：403 / 404（from_scratch 不依赖原构建状态）。
    let resp = rerun(&app, &alice, "sec", 1, r#"{ "mode": "from_scratch" }"#).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 重跑 403");
    let resp = rerun(&app, &dave, "sec", 1, r#"{ "mode": "from_scratch" }"#).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色重跑 404");

    // 不存在构建号 → 404；缺 mode → 422。
    let resp = rerun(&app, &bob, "sec", 999, r#"{ "mode": "from_scratch" }"#).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = rerun(&app, &bob, "sec", 1, "{}").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 mode 422"
    );
}

// ===========================================================================
// 列表
// ===========================================================================

/// AC：列表倒序 + 分页 + 状态过滤 + total；viewer 200 / 无角色 404；非法
// 分页/状态 422。
#[tokio::test]
async fn list_descends_paginates_filters_and_matrix() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", minimal_definition()).await;

    // 触发 3 条；DB 缝分别置 succeeded/failed/queued（倒序 3,2,1）。
    let (b1_id, _) = trigger_build(&app, &bob, "build", "{}").await;
    let (b2_id, _) = trigger_build(&app, &bob, "build", "{}").await;
    let (b3_id, _) = trigger_build(&app, &bob, "build", "{}").await;
    let repo = BuildRepo::new(app.pool.clone());
    repo.transition(b1_id, BuildStatus::Running, 1_000)
        .await
        .expect("1 运行");
    repo.transition(b1_id, BuildStatus::Succeeded, 2_000)
        .await
        .expect("1 成功");
    repo.transition(b2_id, BuildStatus::Running, 3_000)
        .await
        .expect("2 运行");
    repo.transition(b2_id, BuildStatus::Failed, 4_000)
        .await
        .expect("2 失败");
    // b3 留 queued。
    let _ = b3_id;

    // 倒序 + total。
    let resp = list_builds(&app, &bob, "build", "").await;
    assert_eq!(resp.status(), StatusCode::OK, "runner 列表 200");
    let body = body_json(resp).await;
    assert_eq!(body["total"], 3);
    assert_eq!(body["page"], 1);
    assert_eq!(body["limit"], 20, "默认 limit");
    let numbers: Vec<i64> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|b| b["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![3, 2, 1], "按号倒序");
    // 概要字段：状态 + 触发人。
    let item = &body["items"].as_array().expect("items")[1]; // #2 failed
    assert_eq!(item["status"], "failed");
    assert_eq!(item["trigger"], "manual");
    assert_eq!(item["trigger_by"], "bob");

    // 分页：limit=2 page=1 → [3,2]；page=2 → [1]。
    let resp = list_builds(&app, &bob, "build", "?limit=2&page=1").await;
    let body = body_json(resp).await;
    assert_eq!(body["limit"], 2);
    assert_eq!(body["page"], 1);
    let numbers: Vec<i64> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|b| b["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![3, 2]);
    let resp = list_builds(&app, &bob, "build", "?limit=2&page=2").await;
    let body = body_json(resp).await;
    let numbers: Vec<i64> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|b| b["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![1]);

    // 状态过滤：status=failed → [#2]。
    let resp = list_builds(&app, &bob, "build", "?status=failed").await;
    let body = body_json(resp).await;
    assert_eq!(body["total"], 1, "failed total");
    let numbers: Vec<i64> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|b| b["number"].as_i64().expect("number"))
        .collect();
    assert_eq!(numbers, vec![2]);

    // viewer 列表 200（list 是 viewer 档）；无角色 404。
    let resp = list_builds(&app, &alice, "build", "").await;
    assert_eq!(resp.status(), StatusCode::OK, "viewer 列表 200");
    let resp = list_builds(&app, &dave, "build", "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色列表 404");

    // 非法分页/状态 → 422。
    let resp = list_builds(&app, &bob, "build", "?limit=0").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "limit=0 422"
    );
    let resp = list_builds(&app, &bob, "build", "?status=bogus").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad status 422"
    );
}

// ===========================================================================
// 详情 + 缺机密名
// ===========================================================================

/// AC：详情状态/触发人/attempt/耗时/阶段与任务状态；缺机密名任务 → 构建
// failed、任务 detail 记名（不泄值）。
#[tokio::test]
async fn detail_shows_stages_and_missing_secret_records_name() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", definition_with_secret("MISSING_KEY")).await;
    let (build_id, _number) = trigger_build(&app, &bob, "build", "{}").await;

    // 驱动：组装缺机密 → 任务 failed（detail 记名）+ 级联 → 构建 failed。
    drive_build(&app, build_id).await;
    let row = BuildRepo::new(app.pool.clone())
        .get(build_id)
        .await
        .expect("查")
        .expect("应存在");
    assert_eq!(row.status, BuildStatus::Failed, "缺机密构建 failed");

    // 详情：状态 failed、任务 detail 含机密名、无值形态。
    let resp = detail(&app, &bob, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["trigger_by"], "bob");
    assert_eq!(body["attempt"], 1);
    assert!(body["elapsed_ms"].as_i64().is_some(), "已运行有耗时");
    let jobs = body["stages"][0]["jobs"].as_array().expect("jobs");
    let compile = jobs
        .iter()
        .find(|j| j["name"] == "compile")
        .expect("compile 任务");
    assert_eq!(compile["status"], "failed", "缺机密任务 failed");
    let detail_text = compile["detail"].as_str().expect("detail 记名");
    assert!(
        detail_text.contains("MISSING_KEY"),
        "detail 应含缺失机密名：{detail_text}"
    );
    assert!(
        !detail_text.contains("secret") | detail_text.contains("MISSING_KEY"),
        "detail 不含机密值"
    );
    assert_eq!(compile["allow_failure"], false);
    // 响应整体不含任何机密值（缺机密本就无值；结构保证 detail 只记名）。
    let text = serde_json::to_string(&body).expect("序列化");
    assert!(
        !text.contains("super-secret-value"),
        "详情面不得出现机密明文占位"
    );

    // viewer 详情 200（detail 是 viewer 档）；无角色 404（授权矩阵补全）。
    let resp = detail(&app, &alice, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::OK, "viewer 详情 200");
    let resp = detail(&app, &dave, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色详情 404");

    // 不存在构建号 → 404。
    let resp = detail(&app, &bob, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// 删除（手动删构建，票 #78，ADR-0013）
// ===========================================================================

async fn delete_build(app: &TestApp, cookie: &str, pipeline: &str, number: i64) -> Response {
    req_with_cookie(
        app,
        "DELETE",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds/{number}"),
        None,
        Some(cookie),
    )
    .await
}

/// AC（票 #78）：手动删构建——项目 admin 档（viewer/runner 403、无角色 404）；
/// 运行中/排队不可删（409 可读错误）；终态删除 204 + 日志/产物级联删除
/// （builds/jobs 记录保留，ADR-0013 语义）。
#[tokio::test]
async fn delete_build_requires_admin_rejects_live_and_purges_data() {
    let (app, _admin, alice, bob, carol, dave) = fixture().await;
    save_definition(&app, &carol, "build", minimal_definition()).await;

    // 排队构建（尚未 drive）：项目 admin 删 → 409（运行中/排队不可删）。
    let (b_queued, _) = trigger_build(&app, &bob, "build", "{}").await;
    let resp = delete_build(&app, &carol, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "排队中不可删 409");
    assert_eq!(body_json(resp).await["code"], "CONFLICT");

    // 运行中构建（drive 组装后 running）：删 → 409。
    drive_build(&app, b_queued).await;
    let resp = delete_build(&app, &carol, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "运行中不可删 409");

    // 终态化 + 造日志/产物 → 项目 admin 删 → 204 + 级联删除 + 记录保留。
    let repo = BuildRepo::new(app.pool.clone());
    assert!(
        repo.transition(b_queued, BuildStatus::Succeeded, 2_000)
            .await
            .expect("迁移"),
        "置终态"
    );
    // 任务行（drive 已组装）+ 日志 chunk + 产物（磁盘 + 元数据）。
    let job = JobRepo::new(app.pool.clone())
        .list_by_build(b_queued)
        .await
        .expect("任务清单")
        .into_iter()
        .find(|j| j.name == "compile")
        .expect("compile 任务行已由 drive 组装");
    sqlx::query(
        "INSERT INTO logs (build_id, job_id, attempt, start_seq, end_seq, step, stream, data, created_at)
         VALUES (?, ?, 1, 0, 0, -1, '', X'1f8b', ?)",
    )
    .bind(b_queued)
    .bind(job.id)
    .bind(2_000)
    .execute(&app.pool)
    .await
    .expect("插日志");
    // 产物根 = 数据目录 artifacts/（TestApp.web 即数据目录 web/，父目录即数据目录）。
    let artifacts_root = app.web.parent().expect("数据目录").join("artifacts");
    let artifact_dir = artifacts_root.join(b_queued.to_string());
    std::fs::create_dir_all(&artifact_dir).expect("产物目录");
    std::fs::write(artifact_dir.join("dist.bin"), b"bytes").expect("产物文件");
    sqlx::query(
        "INSERT INTO artifacts (build_id, name, path, size, sha256, created_at, retention_until)
         VALUES (?, 'dist.bin', ?, 5, 'abc', 2_000, ?)",
    )
    .bind(b_queued)
    .bind(format!("{b_queued}/dist.bin"))
    .bind(2_000i64 + 30i64 * 24 * 60 * 60 * 1000)
    .execute(&app.pool)
    .await
    .expect("插产物元数据");

    let resp = delete_build(&app, &carol, "build", 1).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "终态删除 204");
    let text = body_text(resp).await;
    assert!(text.is_empty(), "204 无响应体");

    // 级联删除：日志/产物元数据清、产物目录回收；构建记录 + 任务行保留。
    let logs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs WHERE build_id = ?")
        .bind(b_queued)
        .fetch_one(&app.pool)
        .await
        .expect("logs 计数");
    assert_eq!(logs, 0, "日志 chunk 级联删除");
    let metas: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE build_id = ?")
        .bind(b_queued)
        .fetch_one(&app.pool)
        .await
        .expect("artifacts 计数");
    assert_eq!(metas, 0, "产物元数据级联删除");
    assert!(!artifact_dir.exists(), "产物目录回收");
    let builds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM builds WHERE id = ?")
        .bind(b_queued)
        .fetch_one(&app.pool)
        .await
        .expect("builds 计数");
    assert_eq!(builds, 1, "构建记录保留（状态/号/时长可查）");
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE id = ?")
        .bind(job.id)
        .fetch_one(&app.pool)
        .await
        .expect("jobs 计数");
    assert_eq!(jobs, 1, "任务行保留");

    // 授权矩阵：viewer/runner 删 → 403；无角色 → 404（先于状态裁决）。
    let resp = delete_build(&app, &alice, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 删 403");
    let resp = delete_build(&app, &bob, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "runner 删 403");
    let resp = delete_build(&app, &dave, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色删 404");

    // 不存在构建号 → 404（项目 admin）。
    let resp = delete_build(&app, &carol, "build", 999).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

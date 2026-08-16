//! 审计面进程内集成（票 B2b-T7，Router 缝）：事件落库接线、查询端点的
//! 权限矩阵（仅全局 admin，其他角色 403）与过滤/分页行为、detail 形态
//! （机密只记名，永不记值）。只断言 HTTP 状态码与 JSON 形态，不起 socket、
//! 不 spawn 进程。
//!
//! 角色准备沿用 authorization.rs 形态：全局 admin 经 setup wizard，普通
//! 用户直插 + login 换会话。审计事件通过真实端点动作产生（setup / login /
//! 建号 / 建项目 / 配成员 / 机密写入），经 GET /audit 回放断言。

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie};

/// 测试用户共用密码。
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

    login_cookie(app, username, USER_PASSWORD).await
}

/// 指定密码登录换会话 cookie。
async fn login_cookie(app: &TestApp, username: &str, password: &str) -> String {
    let resp = common::post(
        app,
        "/api/v1/auth/login",
        &format!(r#"{{ "username": "{username}", "password": "{password}" }}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "login {username}");
    cookie_of(&resp).expect("会话 cookie")
}

/// GET /audit（带 cookie）。
async fn audit(app: &TestApp, cookie: &str, query: &str) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/audit{query}"),
        None,
        Some(cookie),
    )
    .await
}

/// 取审计清单（断言 200 后解析）。
async fn audit_rows(app: &TestApp, cookie: &str, query: &str) -> Vec<Value> {
    let resp = audit(app, cookie, query).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET /audit 应 200");
    body_json(resp).await.as_array().expect("数组").clone()
}

/// 审计页默认只对全局 admin 开门：非 admin 一律 403（401 之外的面）；
/// 未认证 401（认证中间件全局面，先于授权）。
#[tokio::test]
async fn audit_endpoint_requires_global_admin() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    let alice = user_cookie(&app, "alice").await;
    let bob = user_cookie(&app, "bob").await;
    // 给 alice 一个项目 admin 角色：项目 admin 也不可读全局审计（v1 不
    // 给项目域审计视图，ADR-0015）。
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
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(r#"[ { "username": "alice", "role": "admin" } ]"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "alice 配为项目 admin");

    // 未认证：401。
    let resp = common::req(&app, "GET", "/api/v1/audit", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "未认证 401");

    // 项目 admin（非全局）：403。
    let resp = audit(&app, &alice, "").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "项目 admin 应 403");
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");

    // 无角色普通用户：403。
    let resp = audit(&app, &bob, "").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "普通用户应 403");

    // 全局 admin：200。
    let resp = audit(&app, &admin, "").await;
    assert_eq!(resp.status(), StatusCode::OK, "全局 admin 应 200");
}

/// 端点动作 → 审计事件落库接线：setup 建首个 admin、登录成功、建号、
/// 建项目、配成员、PAT 建/销、机密建/覆写/删、登出——GET /audit 逐类
/// 断言 actor / 事件类型 / project / detail 形态（机密只记名）。
#[tokio::test]
async fn security_events_land_in_audit_with_actor_and_detail_shape() {
    let app = common::test_app().await;
    // setup wizard：user_created（首个 admin，actor 为自身）。
    let admin = common::setup_and_login(&app).await;
    // setup_and_login 内含一次 login 成功：login_success。

    // 全局 admin 建号：user_created（actor = admin，detail 记目标用户名）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/users",
        Some(r#"{ "username": "carol", "password": "carol-pass-1" }"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // carol 登录（login_success）+ 建项目（project_created，carol 是普通
    // 用户不能建——用 admin 建）+ 配成员（member_roles_changed）+ PAT
    // 建/销 + 机密建/覆写/删。全程用 admin 与 carol 各走一段。
    let carol = login_cookie(&app, "carol", "carol-pass-1").await;
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
    assert_eq!(resp.status(), StatusCode::CREATED, "建项目");

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(r#"[ { "username": "carol", "role": "admin" } ]"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "配成员");

    // PAT 建 → 销。
    let pat_resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "ci-deploy" }"#.into()),
        Some(&carol),
    )
    .await;
    assert_eq!(pat_resp.status(), StatusCode::CREATED, "建 PAT");
    let pat_id = body_json(pat_resp).await["id"].as_i64().expect("id");
    let resp = req_with_cookie(
        &app,
        "DELETE",
        &format!("/api/v1/auth/tokens/{pat_id}"),
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "销 PAT");

    // 机密：建 → 覆写 → 删。
    for (name, event) in [("DEPLOY_KEY", "secret_created"), ("DEPLOY_KEY", "secret_overwritten")] {
        let resp = req_with_cookie(
            &app,
            "PUT",
            &format!("/api/v1/projects/demo/secrets/{name}"),
            Some(serde_json::json!({ "value": "v-1" }).to_string()),
            Some(&carol),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{event}");
    }
    let resp = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/projects/demo/secrets/DEPLOY_KEY",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "删机密");

    // 登出（cookie 会话）。
    let resp = req_with_cookie(&app, "POST", "/api/v1/auth/logout", None, Some(&carol)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "登出");

    // 回放全量：逐事件类型断言（时间倒序，只查存在性与形态）。
    let rows = audit_rows(&app, &admin, "").await;
    let event_of = |needle: &str| {
        rows.iter()
            .find(|row| row["event"] == needle)
            .cloned()
            .unwrap_or_else(|| panic!("缺事件 {needle}：{rows:#?}"))
    };

    // 登录成功：actor 为登录用户名。
    let login = event_of("login_success");
    assert_eq!(login["actor"], "carol");
    assert!(login["project"].is_null(), "登录无项目");
    assert!(login["detail"].is_null(), "登录无 detail");

    // 建号（全局 admin 代办）：actor = admin，detail 记目标用户名。
    let created = event_of("user_created");
    assert_eq!(created["actor"], "admin");
    assert_eq!(created["detail"]["username"], "carol");

    // 建项目：project = demo。
    let project = event_of("project_created");
    assert_eq!(project["project"], "demo");
    assert!(project["detail"].is_null());

    // 成员角色变更：project = demo，detail 记清单（用户名 + 角色）。
    let members = event_of("member_roles_changed");
    assert_eq!(members["project"], "demo");
    assert_eq!(members["detail"]["members"][0]["username"], "carol");
    assert_eq!(members["detail"]["members"][0]["role"], "admin");

    // PAT 建/销：detail 记令牌名，永无 token 值。
    assert_eq!(event_of("pat_created")["detail"]["name"], "ci-deploy");
    assert_eq!(event_of("pat_revoked")["detail"]["name"], "ci-deploy");
    let text = serde_json::to_string(&rows).expect("序列化");
    assert!(!text.contains("sis_"), "审计面不得出现令牌值");

    // 机密建/覆写/删：detail 只记名，永不记值。
    let secret_created = event_of("secret_created");
    assert_eq!(secret_created["project"], "demo");
    assert_eq!(secret_created["detail"]["secret"], "DEPLOY_KEY");
    let text = serde_json::to_string(&rows).expect("序列化");
    assert!(!text.contains("v-1"), "审计 detail 只记名，值不得出现");
    assert_eq!(event_of("secret_overwritten")["detail"]["secret"], "DEPLOY_KEY");
    assert_eq!(event_of("secret_deleted")["detail"]["secret"], "DEPLOY_KEY");

    // 登出：actor = carol（cookie 会话通道才记）。
    assert_eq!(event_of("logout")["actor"], "carol");
}

/// 过滤：时间 / 用户 / 项目 / 事件类型 各自生效 + AND 组合；非法事件类型
/// 422 不落查询；分页（limit/offset）切页稳定。
#[tokio::test]
async fn audit_filtering_and_pagination() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    // 造事件：admin 建项目 demo、建项目 other、登录失败（ghost）。
    for name in ["demo", "other"] {
        let resp = req_with_cookie(
            &app,
            "POST",
            "/api/v1/projects",
            Some(format!(
                r#"{{ "name": "{name}", "scm_type": "git", "scm_url": "https://example.com/{name}" }}"#
            )),
            Some(&admin),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "建 {name}");
    }
    let resp = common::post(
        &app,
        "/api/v1/auth/login",
        r#"{ "username": "ghost", "password": "wrong-pass-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "登录失败");

    // 按用户过滤：admin 的 user_created（setup）+ login_success + 两个
    // 建项目（login_failure 的 actor 是 ghost，不在此列）。
    let rows = audit_rows(&app, &admin, "?user=admin").await;
    assert_eq!(rows.len(), 4, "admin 四笔：setup 建号 + 登录 + 两项目");
    assert!(rows.iter().all(|r| r["actor"] == "admin"));

    // 按项目过滤：demo 一笔。
    let rows = audit_rows(&app, &admin, "?project=demo").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["event"], "project_created");

    // 按事件类型过滤：login_failure 一笔（actor 为被尝试的 ghost）。
    let rows = audit_rows(&app, &admin, "?event=login_failure").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["actor"], "ghost");

    // 组合：user=admin & event=project_created → 两笔。
    let rows = audit_rows(&app, &admin, "?user=admin&event=project_created").await;
    assert_eq!(rows.len(), 2, "admin 建两项目");

    // 时间范围：全量首尾 ts 作为 since/until 开合区间（毫秒分辨率下事件
    // 可共享同一 ts，这里以「区间外空、闭区间全量」断言端到端透传；精确
    // 边界语义由 store 缝测试覆盖）。
    let all = audit_rows(&app, &admin, "").await;
    let (min_ts, max_ts) = all.iter().fold((i64::MAX, i64::MIN), |(min, max), r| {
        let ts = r["ts"].as_i64().expect("ts");
        (min.min(ts), max.max(ts))
    });
    let rows = audit_rows(&app, &admin, &format!("?since={min_ts}&until={max_ts}")).await;
    assert_eq!(rows.len(), all.len(), "闭区间应全量");
    let rows = audit_rows(&app, &admin, &format!("?until={}", min_ts - 1)).await;
    assert!(rows.is_empty(), "until 早于最早事件应空");
    let rows = audit_rows(&app, &admin, &format!("?since={}", max_ts + 1)).await;
    assert!(rows.is_empty(), "since 晚于最新事件应空");

    // 非法事件类型：422（不落查询），未知过滤不静默放宽。
    let resp = audit(&app, &admin, "?event=build_started").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "未知事件 422");
    assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");

    // 非法分页：limit=0 / limit=201 422。
    for q in ["?limit=0", "?limit=201", "?offset=-1"] {
        let resp = audit(&app, &admin, q).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{q} 应 422");
    }

    // 分页：limit=2 取最新两笔，offset=2 取下一批；切页不重不漏。
    let page1 = audit_rows(&app, &admin, "?limit=2").await;
    let page2 = audit_rows(&app, &admin, "?limit=2&offset=2").await;
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    let ids: Vec<i64> = page1.iter().chain(&page2).map(|r| r["id"].as_i64().expect("id")).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "时间倒序、id 递减");
    let all_ids: Vec<i64> = all.iter().map(|r| r["id"].as_i64().expect("id")).collect();
    assert_eq!(ids, all_ids[..4], "前两页即前 4 条，与全量一致（不重不漏）");

    // 越界 offset：空数组（不报错）。
    let rows = audit_rows(&app, &admin, "?offset=999").await;
    assert!(rows.is_empty());
}

/// 登录失败事件在审计面可见（限流触达也是失败认证）：5 连败产生 5 笔
/// login_failure（actor = 被尝试的用户名），正确密码 429 也记账。
#[tokio::test]
async fn login_failures_and_rate_limit_land_in_audit() {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

    for _ in 0..5 {
        let resp = common::post(
            &app,
            "/api/v1/auth/login",
            r#"{ "username": "alice", "password": "wrong-pass-1" }"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    // 第 6 次正确密码也 429（限流冷却），同样记账（登录失败事件）。
    let resp = common::post(
        &app,
        "/api/v1/auth/login",
        r#"{ "username": "alice", "password": "user-password-1" }"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    let rows = audit_rows(&app, &admin, "?event=login_failure").await;
    assert_eq!(rows.len(), 6, "5 败 + 1 次限流触达共 6 笔");
    assert!(rows.iter().all(|r| r["actor"] == "alice"));
}

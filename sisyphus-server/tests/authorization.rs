//! 授权面进程内集成（票 B2b-T5，Spec B2b 测试缝）：三档角色矩阵在现有
//! 端点上的可观察行为——401（未认证，全局中间件面，auth_rest 已覆盖）之
//! 外的 404/403/200 三态矩阵、可见性过滤、成员管理即时生效与用户目录
//! 守卫。只断言 HTTP 状态码与 JSON 形态，不起 socket、不 spawn 进程。
//!
//! 非 admin 用户尚无自建端点（用户管理随 T4 批次），用例经连接池直插
//! 用户行（共用一个 argon2 PHC，密码统一 `user-password-1`）再走 login
//! 换会话——行为面与真实用户完全一致。

use axum::http::StatusCode;
use axum::response::Response;
use sqlx::SqlitePool;

mod common;

use common::{TestApp, body_json, cookie_of, req_with_cookie};

/// 测试用户共用密码（与共用 PHC 对应）。
const USER_PASSWORD: &str = "user-password-1";

/// 直插一个非 admin 用户并 login 换会话 cookie（返回 cookie 头值）。
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

/// 装配 + 全局 admin（setup wizard）+ 项目 + 三档成员。
///
/// 返回 (app, admin cookie, alice=viewer / bob=runner / carol=admin 的 cookie，
/// dave=无角色 的 cookie)。成员由全局 admin 显式分配（隐含 admin 不落表）。
async fn fixture_with_roles(project: &str) -> (TestApp, String, String, String, String, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;
    create_project(&app, &admin, project).await;

    let (alice, bob, carol, dave) = (
        user_cookie(&app, "alice").await,
        user_cookie(&app, "bob").await,
        user_cookie(&app, "carol").await,
        user_cookie(&app, "dave").await,
    );
    assign_members(
        &app,
        &admin,
        project,
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

/// 全局 admin 整组分配成员（隐含项目 admin，无需成员行）。
async fn assign_members(app: &TestApp, admin: &str, project: &str, body: &str) {
    let resp = req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/{project}/members"),
        Some(body.into()),
        Some(admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "分配成员应 200");
}

/// 最小合法 Pipeline 定义。
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

async fn put_definition(app: &TestApp, cookie: &str, project: &str) -> Response {
    req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/{project}/pipelines/build"),
        Some(valid_definition()),
        Some(cookie),
    )
    .await
}

async fn get_definition(app: &TestApp, cookie: &str, project: &str) -> Response {
    req_with_cookie(
        app,
        "GET",
        &format!("/api/v1/projects/{project}/pipelines/build"),
        None,
        Some(cookie),
    )
    .await
}

/// 三档矩阵在定义端点上的 AC：viewer GET 200 / PUT 403；runner GET 200 /
/// PUT 403；admin PUT 200；全局 admin 不配成员即可 PUT（隐含项目 admin）。
#[tokio::test]
async fn role_matrix_on_definition_endpoints() {
    let (app, admin, alice, bob, carol, _dave) = fixture_with_roles("demo").await;

    // 基线定义由全局 admin 首存（也验证隐含 admin 的 PUT 面）。
    let resp = put_definition(&app, &admin, "demo").await;
    assert_eq!(resp.status(), StatusCode::OK, "全局 admin 无成员行可 PUT");
    assert_eq!(body_json(resp).await["operator"], "admin");

    // viewer：GET 200 / PUT 403。
    let resp = get_definition(&app, &alice, "demo").await;
    assert_eq!(resp.status(), StatusCode::OK, "viewer GET 200");
    let resp = put_definition(&app, &alice, "demo").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer PUT 403");
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");

    // runner：GET 200 / PUT 403。
    let resp = get_definition(&app, &bob, "demo").await;
    assert_eq!(resp.status(), StatusCode::OK, "runner GET 200");
    let resp = put_definition(&app, &bob, "demo").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "runner PUT 403");

    // 项目 admin：PUT 200，revision 递增、操作人实名落库。
    let resp = put_definition(&app, &carol, "demo").await;
    assert_eq!(resp.status(), StatusCode::OK, "admin PUT 200");
    let saved = body_json(resp).await;
    assert_eq!(saved["revision"], 2, "续存 revision 递增");
    assert_eq!(saved["operator"], "carol", "操作人为认证用户实名");

    // 读回路径（GET）也带实名操作人。
    let resp = get_definition(&app, &alice, "demo").await;
    assert_eq!(body_json(resp).await["operator"], "carol");
}

/// 无角色已登录用户：项目列表不含、单查与定义读写都 404（不泄存在性）；
/// 非全局 admin 建项目 403；普通用户列表只含有角色的项目。
#[tokio::test]
async fn visibility_filtering_and_no_existence_leak() {
    let (app, admin, alice, _bob, carol, dave) = fixture_with_roles("demo").await;
    create_project(&app, &admin, "other").await;

    // 普通用户只列有角色的项目；全局 admin 全量。
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&alice)).await;
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("清单")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["demo"], "viewer 只见有角色的项目");
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&dave)).await;
    assert_eq!(
        body_json(resp).await,
        serde_json::json!([]),
        "无角色清单为空"
    );
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects", None, Some(&admin)).await;
    let count = body_json(resp).await.as_array().expect("清单").len();
    assert_eq!(count, 2, "全局 admin 全量");

    // 无角色：已存在项目单查 404（与不存在同形）、定义读写 404。
    for path in [
        "/api/v1/projects/other",
        "/api/v1/projects/other/pipelines/build",
    ] {
        let resp = req_with_cookie(&app, "GET", path, None, Some(&dave)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色 GET {path}");
    }
    let resp = put_definition(&app, &dave, "other").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色 PUT 404");
    // 同项目内无角色不存在（dave 对 demo 无角色）：
    let resp = get_definition(&app, &dave, "demo").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "无角色 GET 定义 404");

    // 真不存在的项目：同为 404（与无角色不可分辨）。
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects/ghost", None, Some(&dave)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 非全局 admin 建项目 403（全局资源只认 is_admin）。
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects",
        Some(
            r#"{ "name": "mine", "scm_type": "git", "scm_url": "https://example.com/mine" }"#
                .into(),
        ),
        Some(&carol),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "项目 admin 建项目仍 403"
    );
    assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
}

/// 成员管理：项目 admin 查看 / 整组分配（分配即时生效、移除即时失去）；
/// viewer / runner 403；输入校验（不存在 / 重复 / 空用户名）422。
#[tokio::test]
async fn member_management_takes_effect_immediately() {
    let (app, _admin, alice, bob, carol, dave) = fixture_with_roles("demo").await;

    // 项目 admin（carol）可查看与分配；分配 dave runner 后即刻有权限。
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/members",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let members = body_json(resp).await;
    let roles: Vec<(String, String)> = members
        .as_array()
        .expect("清单")
        .iter()
        .map(|m| {
            (
                m["username"].as_str().expect("username").to_string(),
                m["role"].as_str().expect("role").to_string(),
            )
        })
        .collect();
    let expected = [
        ("alice", "viewer"),
        ("bob", "runner"),
        ("carol", "admin"),
    ];
    let roles: Vec<(&str, &str)> = roles
        .iter()
        .map(|(u, r)| (u.as_str(), r.as_str()))
        .collect();
    assert_eq!(roles, expected, "成员按用户名排序整组可见");

    assign_members(
        &app,
        &carol,
        "demo",
        r#"[ { "username": "alice", "role": "viewer" },
             { "username": "bob", "role": "runner" },
             { "username": "carol", "role": "admin" },
             { "username": "dave", "role": "runner" } ]"#,
    )
    .await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects/demo", None, Some(&dave)).await;
    assert_eq!(resp.status(), StatusCode::OK, "分配后 dave 即时可查项目");

    // 移除 alice：下一请求即 404（整组替换语义）；随后加回 viewer 供
    // 下面的档位矩阵断言（carol 仍 admin）。
    assign_members(
        &app,
        &carol,
        "demo",
        r#"[ { "username": "bob", "role": "runner" },
             { "username": "carol", "role": "admin" },
             { "username": "dave", "role": "runner" } ]"#,
    )
    .await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/projects/demo", None, Some(&alice)).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "移除后 alice 即时 404"
    );
    assign_members(
        &app,
        &carol,
        "demo",
        r#"[ { "username": "alice", "role": "viewer" },
             { "username": "bob", "role": "runner" },
             { "username": "carol", "role": "admin" },
             { "username": "dave", "role": "runner" } ]"#,
    )
    .await;

    // viewer / runner 对成员端点 403（档位不足，非 404——角色在）。
    for cookie in [&alice, &bob] {
        let resp = req_with_cookie(
            &app,
            "GET",
            "/api/v1/projects/demo/members",
            None,
            Some(cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET members 403");
        let resp = req_with_cookie(
            &app,
            "PUT",
            "/api/v1/projects/demo/members",
            Some(r#"[{ "username": "alice", "role": "admin" }]"#.into()),
            Some(cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "PUT members 403");
    }

    // 校验面：用户不存在 / 重复 / 空用户名 → 422 清单整组，且不落任何变更。
    for body in [
        r#"[ { "username": "ghost-user", "role": "viewer" } ]"#,
        r#"[ { "username": "bob", "role": "viewer" }, { "username": "bob", "role": "admin" } ]"#,
        r#"[ { "username": "  ", "role": "viewer" } ]"#,
    ] {
        let resp = req_with_cookie(
            &app,
            "PUT",
            "/api/v1/projects/demo/members",
            Some(body.into()),
            Some(&carol),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let error = body_json(resp).await;
        assert_eq!(error["code"], "VALIDATION_FAILED");
        assert!(
            error["detail"]["errors"]
                .as_array()
                .is_some_and(|e| !e.is_empty()),
            "错误清单非空：{error}"
        );
    }
    // 上面的非法提交未改变成员状态（bob 仍 runner、carol 仍 admin）。
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/members",
        None,
        Some(&carol),
    )
    .await;
    let members = body_json(resp).await;
    assert_eq!(
        members
            .as_array()
            .expect("清单")
            .iter()
            .filter(|m| m["username"] == "bob")
            .count(),
        1,
        "非法提交不落变更"
    );

    // 项目 admin 自降（carol 移除自己）：下一请求即 404（全局 admin 仍可救）。
    assign_members(
        &app,
        &carol,
        "demo",
        r#"[{ "username": "bob", "role": "runner" }]"#,
    )
    .await;
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/members",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "自降后 carol 即时 404"
    );
}

/// 用户目录：项目 admin 可读（仅 id + username）；viewer / runner / 无角色
/// 403；全局 admin 200；禁用用户不在目录。
#[tokio::test]
async fn user_directory_gated_to_project_admins() {
    let (app, admin, alice, bob, carol, dave) = fixture_with_roles("demo").await;

    // 禁用用户不入目录（直插 disabled 行）。
    sqlx::query(
        "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
         VALUES ('ex-employee', 'x', 0, 1, 1, 1)",
    )
    .execute(&app.pool)
    .await
    .expect("直插禁用用户");

    let resp = req_with_cookie(&app, "GET", "/api/v1/users/directory", None, Some(&carol)).await;
    assert_eq!(resp.status(), StatusCode::OK, "项目 admin 可读目录");
    let entries = body_json(resp).await;
    let entries = entries.as_array().expect("目录为数组");
    assert_eq!(
        entries.len(),
        5,
        "5 个活跃用户（admin + alice/bob/carol/dave）"
    );
    for entry in entries {
        let keys: Vec<&str> = entry
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["id", "username"], "最小目录：仅 id + username");
        assert!(entry["id"].as_i64().is_some());
        assert!(entry["username"].as_str().is_some());
    }
    assert!(
        !entries.iter().any(|e| e["username"] == "ex-employee"),
        "禁用用户不入目录"
    );

    // 全局 admin 同样可读。
    let resp = req_with_cookie(&app, "GET", "/api/v1/users/directory", None, Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK, "全局 admin 可读目录");

    // viewer / runner / 无角色：403。
    for cookie in [&alice, &bob, &dave] {
        let resp =
            req_with_cookie(&app, "GET", "/api/v1/users/directory", None, Some(cookie)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "非项目 admin 403");
        assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
    }
}

/// Bearer PAT 的授权 = owner 本人：viewer 的 PAT 在定义保存端点同样 403
/// （授权 extractor 只看角色，与认证通道无关）。
#[tokio::test]
async fn pat_carries_owner_role_into_authorization() {
    let (app, _admin, alice, _bob, _carol, _dave) = fixture_with_roles("demo").await;

    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/auth/tokens",
        Some(r#"{ "name": "ci" }"#.into()),
        Some(&alice),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let token = body_json(resp).await["token"]
        .as_str()
        .expect("一次性令牌")
        .to_string();

    let resp = common::custom_req(
        &app,
        "PUT",
        "/api/v1/projects/demo/pipelines/build",
        Some(valid_definition()),
        None,
        &[("authorization", format!("Bearer {token}"))],
        common::DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "viewer 的 PAT 仍 403");

    let resp = common::custom_req(
        &app,
        "GET",
        "/api/v1/projects/demo",
        None,
        None,
        &[("authorization", format!("Bearer {token}"))],
        common::DEFAULT_PEER,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "viewer 的 PAT GET 200");
}

/// 直插成员行的旧状态不受影响（回归护栏）：共享池上显式成员与隐含 admin
/// 并存时，role_of 判定以成员表为准、is_admin 只升不降。
#[tokio::test]
async fn explicit_member_row_wins_for_regular_users() {
    let (app, _admin, _alice, _bob, carol, _dave) = fixture_with_roles("demo").await;
    let pool: &SqlitePool = &app.pool;
    let carol_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'carol'")
        .fetch_one(pool)
        .await
        .expect("carol 行");
    let demo_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(pool)
        .await
        .expect("demo 行");
    let role: String =
        sqlx::query_scalar("SELECT role FROM project_members WHERE project_id = ? AND user_id = ?")
            .bind(demo_id)
            .bind(carol_id)
            .fetch_one(pool)
            .await
            .expect("成员行");
    assert_eq!(role, "admin", "显式成员行按分配落库（隐含 admin 不落表）");

    let resp = req_with_cookie(&app, "GET", "/api/v1/projects/demo", None, Some(&carol)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

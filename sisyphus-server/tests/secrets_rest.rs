//! 机密面进程内集成（票 B2b-T6，Spec B2b 测试缝）：密钥文件首启语义、
//! 三端点行为与档位/存在性矩阵——值只写不读（GET 面走遍无读值路径）、
//! viewer/runner 连名 403、非法名 422、密文落库形态。只断言 HTTP 状态码
//! 与 JSON 形态，不起 socket、不 spawn 进程。
//!
//! 角色准备沿用 authorization.rs 的直插形态：全局 admin 经 setup wizard
//! （建项目、配成员），普通用户直插 + login 换会话。

use axum::http::StatusCode;
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

/// 装配 + 全局 admin（setup wizard）+ 项目 demo + 三档成员（viewer alice /
/// runner bob / admin carol）。
async fn fixture() -> (TestApp, String, String, String, String) {
    let app = common::test_app().await;
    let admin = common::setup_and_login(&app).await;

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

    let (alice, bob, carol) = (
        user_cookie(&app, "alice").await,
        user_cookie(&app, "bob").await,
        user_cookie(&app, "carol").await,
    );
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
    (app, admin, alice, bob, carol)
}

fn put_secret_body(value: &str) -> String {
    serde_json::json!({ "value": value }).to_string()
}

/// 机密面全矩阵：viewer/runner 访问三端点一律 403（连名都不列——403 先于
/// 任何查询）；项目 admin 与全局 admin 可建/列/删；值只写不读（GET 面任何
/// 响应无值）。
#[tokio::test]
async fn secrets_matrix_viewer_runner_403_and_write_path_only() {
    let (app, _admin, alice, bob, carol) = fixture().await;

    // viewer / runner：列名、建、删全部 403（档位不足，非 404——角色在）。
    for cookie in [&alice, &bob] {
        let resp = req_with_cookie(
            &app,
            "GET",
            "/api/v1/projects/demo/secrets",
            None,
            Some(cookie),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "viewer/runner GET 403"
        );
        assert_eq!(body_json(resp).await["code"], "FORBIDDEN");
        let resp = req_with_cookie(
            &app,
            "PUT",
            "/api/v1/projects/demo/secrets/TOKEN",
            Some(put_secret_body("value-1")),
            Some(cookie),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "viewer/runner PUT 403"
        );
        let resp = req_with_cookie(
            &app,
            "DELETE",
            "/api/v1/projects/demo/secrets/TOKEN",
            None,
            Some(cookie),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "viewer/runner DELETE 403"
        );
    }

    // 项目 admin：建 → 列名 → 删除 → 名消失 全链路。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/secrets/DEPLOY_KEY",
        Some(put_secret_body("very-secret-value")),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "admin 建机密 204");

    // GET 仅名清单：响应只有名、永无值形态。
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "admin 列名 200");
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("清单")
        .iter()
        .map(|s| s["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["DEPLOY_KEY"], "仅名、按名排序");
    let text = serde_json::to_string(&list).expect("序列化");
    assert!(
        !text.contains("very-secret-value"),
        "值不得出现在任何 GET 响应"
    );

    // 覆写同名校：204（值不回显），名字仍只一个（唯一键覆写语义）。
    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/secrets/DEPLOY_KEY",
        Some(put_secret_body("rotated-value")),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "覆写 204");
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(
        body_json(resp).await.as_array().expect("清单").len(),
        1,
        "覆写语义：名唯一"
    );

    // 删除 → 名消失；再删同名校 404。
    let resp = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/projects/demo/secrets/DEPLOY_KEY",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "删除 204");
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(
        body_json(resp).await,
        serde_json::json!([]),
        "DELETE 后名消失"
    );
    let resp = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/projects/demo/secrets/DEPLOY_KEY",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "再删 404");
}

/// 全局 admin 隐含项目 admin：无成员行即可管理机密（与成员面同纪律）。
#[tokio::test]
async fn global_admin_manages_secrets_without_membership_row() {
    let (app, admin, _alice, _bob, _carol) = fixture().await;

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/secrets/GLOBAL",
        Some(put_secret_body("admin-value")),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "全局 admin 建机密");
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&admin),
    )
    .await;
    let list = body_json(resp).await;
    let names: Vec<&str> = list
        .as_array()
        .expect("清单")
        .iter()
        .map(|s| s["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["GLOBAL"], "全局 admin 可见全部机密名");
}

/// 机密名校验：空名 / 非法字符 422；合法字符集（字母数字 + 下划线）可写。
#[tokio::test]
async fn secret_name_validation_rejects_invalid_names() {
    let (app, _admin, _alice, _bob, carol) = fixture().await;

    // 非法字符（URL 路径段百分号编码后仍被解码进机密名）：422；空名由
    // 路由结构天然不命中（空路径段无路由，404），空名校验规则见 charset 单测。
    for bad in [
        "%20",
        "has-dash",
        "has.dot",
        "has%20space",
        "%E6%97%A5%E6%9C%AC%E8%AA%9E",
    ] {
        let resp = req_with_cookie(
            &app,
            "PUT",
            &format!("/api/v1/projects/demo/secrets/{bad}"),
            Some(put_secret_body("v")),
            Some(&carol),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "非法名 {bad:?} 应 422"
        );
        assert_eq!(body_json(resp).await["code"], "VALIDATION_FAILED");
    }
    // 非法提交不落任何写入（名清单保持空）。
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/secrets",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(body_json(resp).await, serde_json::json!([]));

    // DELETE 同走名校验（端点族规则统一）：非法名 422，先于「不存在」404。
    let resp = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/projects/demo/secrets/has-dash",
        None,
        Some(&carol),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "DELETE 非法名 422"
    );

    // 合法名（下划线、数字）：可写。
    for name in ["DEPLOY_KEY", "NPM_TOKEN_2FA", "_private"] {
        let resp = req_with_cookie(
            &app,
            "PUT",
            &format!("/api/v1/projects/demo/secrets/{name}"),
            Some(put_secret_body("v")),
            Some(&carol),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "合法名 {name}");
    }
}

/// 机密值加密落库：store 缝直查临时库为「版本字节 + nonce + 密文」形态、
/// 与明文不等、明文不出现在库内任何列；DELETE 后行消失。
#[tokio::test]
async fn ciphertext_is_stored_in_blob_form_not_plaintext() {
    let (app, _admin, _alice, _bob, carol) = fixture().await;
    let pool: &SqlitePool = &app.pool;
    let plaintext = "super-secret-db-value";

    let resp = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/secrets/DB",
        Some(put_secret_body(plaintext)),
        Some(&carol),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "写入");

    let demo_id: i64 = sqlx::query_scalar("SELECT id FROM projects WHERE name = 'demo'")
        .fetch_one(pool)
        .await
        .expect("demo 行");
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM secrets WHERE project_id = ? AND name = 'DB'")
            .bind(demo_id)
            .fetch_one(pool)
            .await
            .expect("直查密文");
    assert_eq!(
        stored[0],
        sisyphus_server::secrets::CIPHERTEXT_VERSION,
        "首字节为版本字节"
    );
    assert!(
        stored.len() > sisyphus_server::secrets::NONCE_LEN,
        "版本字节 + nonce + 密文"
    );
    let stored_text = String::from_utf8_lossy(&stored);
    assert!(
        !stored_text.contains(plaintext),
        "密文不得含明文（直查形态）"
    );

    // 全库扫描：明文值不得出现在任何列（库泄露读不出凭据）。
    let hits: Vec<String> = sqlx::query_scalar("SELECT name FROM secrets WHERE ciphertext LIKE ?")
        .bind(format!("%{plaintext}%"))
        .fetch_all(pool)
        .await
        .expect("扫描");
    assert!(hits.is_empty(), "明文不得作为子串出现：{hits:?}");

    // 操作人实名落库（认证用户名，票 B2b-T5 纪律）。
    let updated_by: String =
        sqlx::query_scalar("SELECT updated_by FROM secrets WHERE project_id = ? AND name = 'DB'")
            .bind(demo_id)
            .fetch_one(pool)
            .await
            .expect("操作人");
    assert_eq!(updated_by, "carol", "操作人为认证用户实名");
}

/// 无角色已登录用户与真不存在的项目：机密端点 404（与项目域同形，不泄
/// 存在性）；未认证 401（认证中间件全局面）。
#[tokio::test]
async fn secrets_no_role_404_ghost_404_unauth_401() {
    let (app, _admin, _alice, _bob, _carol) = fixture().await;
    let dave = user_cookie(&app, "dave").await; // 无角色

    // 无角色：项目不可见 → 机密端点 404（与「项目不存在」同形）。
    for (method, path, body) in [
        ("GET", "/api/v1/projects/demo/secrets", None),
        (
            "PUT",
            "/api/v1/projects/demo/secrets/X",
            Some(put_secret_body("v")),
        ),
        ("DELETE", "/api/v1/projects/demo/secrets/X", None),
    ] {
        let resp = req_with_cookie(&app, method, path, body, Some(&dave)).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} 无角色 404"
        );
    }

    // 真不存在的项目：同为 404（与无角色不可分辨）。
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/ghost/secrets",
        None,
        Some(&dave),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 未认证：401（认证中间件全局面，先于授权）。
    for (method, path, body) in [
        ("GET", "/api/v1/projects/demo/secrets", None),
        (
            "PUT",
            "/api/v1/projects/demo/secrets/X",
            Some(put_secret_body("v")),
        ),
        ("DELETE", "/api/v1/projects/demo/secrets/X", None),
    ] {
        let resp = common::req(&app, method, path, body).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} 未认证 401"
        );
    }
}

/// 机密名校验规则（env 键字符集）：非空、字母数字与下划线；实现委托
/// sisyphus-model 的变量名校验（单一事实源——机密经 env 注入，键名与
/// 变量名同字符集）。
#[test]
fn secret_name_charset_rule() {
    for name in ["DEPLOY_KEY", "NPM_TOKEN_2FA", "_private", "a1"] {
        assert!(
            sisyphus_server::api::secrets::is_valid_secret_name(name),
            "{name} 应合法"
        );
    }
    for name in ["", " ", "has-dash", "has.dot", "日本語"] {
        assert!(
            !sisyphus_server::api::secrets::is_valid_secret_name(name),
            "{name:?} 应非法"
        );
    }
}

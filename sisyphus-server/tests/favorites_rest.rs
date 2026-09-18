//! 流水线收藏 REST 集成测试（票 #137）。
//!
//! 只经生产 Router + 真实 SQLite 临时库观察公开 HTTP 行为，不触碰仓储内部。

use axum::http::StatusCode;
use futures::future::join_all;

mod common;

use common::{body_json, req_with_cookie, setup_and_login, test_app};
use sisyphus_server::store::builds::{BuildRepo, BuildStatus};

async fn login_as_regular(app: &common::TestApp, admin_cookie: &str, username: &str) -> String {
    let created = req_with_cookie(
        app,
        "POST",
        "/api/v1/users",
        Some(format!(
            r#"{{"username":"{username}","password":"user-password-12"}}"#
        )),
        Some(admin_cookie),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let logged_in = req_with_cookie(
        app,
        "POST",
        "/api/v1/auth/login",
        Some(format!(
            r#"{{"username":"{username}","password":"user-password-12"}}"#
        )),
        None,
    )
    .await;
    assert_eq!(logged_in.status(), StatusCode::OK);
    common::cookie_of(&logged_in).expect("登录应下发 cookie")
}

async fn create_project(app: &common::TestApp, cookie: &str) {
    let response = req_with_cookie(
        app,
        "POST",
        "/api/v1/projects",
        Some(r#"{"name":"demo","scm_type":"git","scm_url":"https://example.com/demo"}"#.into()),
        Some(cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

async fn save_pipeline(app: &common::TestApp, cookie: &str, pipeline: &str) {
    let definition = serde_json::json!({
        "name": pipeline,
        "stages": [{
            "name": "build",
            "jobs": [{
                "name": "compile",
                "steps": [{"type": "shell", "config": {"command": "echo ok"}}]
            }]
        }]
    });
    let response = req_with_cookie(
        app,
        "PUT",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}"),
        Some(definition.to_string()),
        Some(cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn authenticated_user_starts_with_empty_favorites() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;

    let response = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, serde_json::json!([]));
}

#[tokio::test]
async fn adding_existing_pipeline_persists_it_in_the_current_users_list() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;

    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let favorites = body_json(listed).await;
    assert_eq!(favorites.as_array().map(Vec::len), Some(1));
    assert_eq!(favorites[0]["project"], "demo");
    assert_eq!(favorites[0]["pipeline"], "main");
    assert!(favorites[0]["added_at"].as_i64().is_some_and(|at| at > 0));
    assert!(favorites[0]["latest_build"].is_null());
}

#[tokio::test]
async fn adding_the_same_favorite_twice_is_idempotent() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;

    for _ in 0..2 {
        let response = req_with_cookie(
            &app,
            "PUT",
            "/api/v1/user/pipeline-favorites/demo/main",
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(body_json(listed).await.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn removing_a_favorite_is_idempotent() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;

    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);

    for _ in 0..2 {
        let removed = req_with_cookie(
            &app,
            "DELETE",
            "/api/v1/user/pipeline-favorites/demo/main",
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    }

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(body_json(listed).await, serde_json::json!([]));
}

#[tokio::test]
async fn favorite_requires_project_visibility_and_is_private_to_its_owner() {
    let app = test_app().await;
    let admin = setup_and_login(&app).await;
    create_project(&app, &admin).await;
    save_pipeline(&app, &admin, "main").await;
    let alice = login_as_regular(&app, &admin, "alice").await;

    let hidden = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(hidden).await["code"], "NOT_FOUND");

    let assigned = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(r#"[{"username":"alice","role":"viewer"}]"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(assigned.status(), StatusCode::OK);

    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);

    let alice_list = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(
        body_json(alice_list).await.as_array().map(Vec::len),
        Some(1)
    );

    let admin_list = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&admin),
    )
    .await;
    assert_eq!(body_json(admin_list).await, serde_json::json!([]));
}

#[tokio::test]
async fn favorite_becomes_hidden_when_project_access_is_revoked() {
    let app = test_app().await;
    let admin = setup_and_login(&app).await;
    create_project(&app, &admin).await;
    save_pipeline(&app, &admin, "main").await;
    let alice = login_as_regular(&app, &admin, "alice").await;

    let assigned = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some(r#"[{"username":"alice","role":"viewer"}]"#.into()),
        Some(&admin),
    )
    .await;
    assert_eq!(assigned.status(), StatusCode::OK);
    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);

    let revoked = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/projects/demo/members",
        Some("[]".into()),
        Some(&admin),
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::OK);

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(body_json(listed).await, serde_json::json!([]));
}

#[tokio::test]
async fn favorite_list_tracks_the_latest_real_build() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;
    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);

    let triggered = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/demo/pipelines/main/builds",
        Some("{}".into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(triggered.status(), StatusCode::ACCEPTED);
    let build_id = body_json(triggered).await["build_id"]
        .as_i64()
        .expect("触发响应含 build_id");

    let repo = BuildRepo::new(app.pool.clone());
    assert!(
        repo.transition(build_id, BuildStatus::Running, 1_000)
            .await
            .expect("进入运行态")
    );
    assert!(
        repo.transition(build_id, BuildStatus::Succeeded, 2_500)
            .await
            .expect("进入成功终态")
    );

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    let latest = &body_json(listed).await[0]["latest_build"];
    assert_eq!(latest["number"], 1);
    assert_eq!(latest["status"], "succeeded");
    assert_eq!(latest["started_at"], 1_000);
    assert_eq!(latest["finished_at"], 2_500);

    let triggered_again = req_with_cookie(
        &app,
        "POST",
        "/api/v1/projects/demo/pipelines/main/builds",
        Some("{}".into()),
        Some(&cookie),
    )
    .await;
    assert_eq!(triggered_again.status(), StatusCode::ACCEPTED);

    let listed_again = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    let latest = &body_json(listed_again).await[0]["latest_build"];
    assert_eq!(latest["number"], 2);
    assert_eq!(latest["status"], "queued");
    assert_eq!(latest["started_at"], serde_json::Value::Null);
    assert_eq!(latest["finished_at"], serde_json::Value::Null);
}

#[tokio::test]
async fn concurrent_duplicate_adds_create_one_favorite() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;

    let responses = join_all((0..8).map(|_| {
        req_with_cookie(
            &app,
            "PUT",
            "/api/v1/user/pipeline-favorites/demo/main",
            None,
            Some(&cookie),
        )
    }))
    .await;
    assert!(
        responses
            .iter()
            .all(|response| response.status() == StatusCode::NO_CONTENT)
    );

    let listed = req_with_cookie(
        &app,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(body_json(listed).await.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn favorites_survive_server_reassembly() {
    let dir = tempfile::tempdir().expect("临时数据目录");
    let app = common::test_app_at(dir.path()).await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;
    let added = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/main",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);
    drop(app);

    let restarted = common::test_app_at(dir.path()).await;
    let listed = req_with_cookie(
        &restarted,
        "GET",
        "/api/v1/user/pipeline-favorites",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(body_json(listed).await[0]["pipeline"], "main");
}

#[tokio::test]
async fn add_rejects_missing_pipeline_and_all_endpoints_require_authentication() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;

    let missing = req_with_cookie(
        &app,
        "PUT",
        "/api/v1/user/pipeline-favorites/demo/missing",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(missing).await["code"], "NOT_FOUND");

    let missing = req_with_cookie(
        &app,
        "DELETE",
        "/api/v1/user/pipeline-favorites/demo/missing",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(missing).await["code"], "NOT_FOUND");

    for (method, path) in [
        ("GET", "/api/v1/user/pipeline-favorites"),
        ("PUT", "/api/v1/user/pipeline-favorites/demo/main"),
        ("DELETE", "/api/v1/user/pipeline-favorites/demo/main"),
    ] {
        let response = req_with_cookie(&app, method, path, None, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

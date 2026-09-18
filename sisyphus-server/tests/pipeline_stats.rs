//! 流水线统计端点集成测试（#135）：真实 builds 行聚合、窗口边界、空态与
//! pipeline/认证错误语义。

use axum::http::StatusCode;

mod common;

use common::{body_json, req_with_cookie, setup_and_login, test_app};
use sisyphus_server::store::builds::{BuildRepo, BuildStatus};

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
    let body = serde_json::json!({
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
        Some(body.to_string()),
        Some(cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn trigger(app: &common::TestApp, cookie: &str, pipeline: &str) -> i64 {
    let response = req_with_cookie(
        app,
        "POST",
        &format!("/api/v1/projects/demo/pipelines/{pipeline}/builds"),
        Some("{}".into()),
        Some(cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    body_json(response).await["build_id"]
        .as_i64()
        .expect("build_id")
}

#[tokio::test]
async fn stats_aggregate_real_build_rows_and_empty_terminal_values() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;
    save_pipeline(&app, &cookie, "empty").await;

    let first = trigger(&app, &cookie, "main").await;
    let second = trigger(&app, &cookie, "main").await;
    let _queued = trigger(&app, &cookie, "main").await;
    let builds = BuildRepo::new(app.pool.clone());
    assert!(
        builds
            .transition(first, BuildStatus::Running, 1_000)
            .await
            .expect("first running")
    );
    assert!(
        builds
            .transition(first, BuildStatus::Succeeded, 2_000)
            .await
            .expect("first succeeded")
    );
    assert!(
        builds
            .transition(second, BuildStatus::Running, 3_000)
            .await
            .expect("second running")
    );
    assert!(
        builds
            .transition(second, BuildStatus::Failed, 4_500)
            .await
            .expect("second failed")
    );

    let response = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/pipelines/main/stats?window=2",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let stats = body_json(response).await;
    assert_eq!(stats["window"], 2);
    assert_eq!(stats["total_builds"], 3);
    assert_eq!(stats["terminal_count"], 1);
    assert_eq!(stats["succeeded_count"], 0);
    assert_eq!(stats["success_rate"], 0.0);
    assert_eq!(stats["avg_duration_ms"], 1_500);
    assert_eq!(stats["latest_build"]["number"], 3);
    assert_eq!(stats["latest_build"]["status"], "queued");

    let response = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/pipelines/empty/stats?window=999",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let empty = body_json(response).await;
    assert_eq!(empty["window"], 0);
    assert_eq!(empty["total_builds"], 0);
    assert!(empty["success_rate"].is_null());
    assert!(empty["avg_duration_ms"].is_null());
    assert!(empty["latest_build"].is_null());
}

#[tokio::test]
async fn stats_clamp_window_and_reject_missing_pipeline_or_auth() {
    let app = test_app().await;
    let unauthenticated = common::get(&app, "/api/v1/projects/demo/pipelines/main/stats").await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let cookie = setup_and_login(&app).await;
    create_project(&app, &cookie).await;
    save_pipeline(&app, &cookie, "main").await;
    let _ = trigger(&app, &cookie, "main").await;
    let response = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/pipelines/main/stats?window=999",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["window"], 1);

    let missing = req_with_cookie(
        &app,
        "GET",
        "/api/v1/projects/demo/pipelines/missing/stats",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

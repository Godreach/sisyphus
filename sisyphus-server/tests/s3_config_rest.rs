//! 票 #122：未配置 S3 时制品库入口不可用、凭据不进 API、本地产物不受影响。

mod common;

use common::{body_json, req_with_cookie, setup_and_login, test_app};

#[tokio::test]
async fn artifact_repository_is_unavailable_without_s3() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let resp = req_with_cookie(
        &app,
        "GET",
        "/api/v1/artifact-repository",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    assert_eq!(body["available"], false);
    assert_eq!(body["reason"], "s3_unconfigured");
    assert!(body.get("backend").is_none() || body["backend"].is_null());
}

#[tokio::test]
async fn s3_config_get_is_redacted_and_unconfigured() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let resp = req_with_cookie(&app, "GET", "/api/v1/config/s3", None, Some(&cookie)).await;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    assert_eq!(body["configured"], false);
    let dump = body.to_string();
    assert!(!dump.contains("secret"), "{dump}");
    assert!(!dump.contains("access_key"), "{dump}");
}

#[tokio::test]
async fn s3_test_connection_without_config_is_conflict() {
    let app = test_app().await;
    let cookie = setup_and_login(&app).await;
    let resp = req_with_cookie(
        &app,
        "POST",
        "/api/v1/config/s3/test-connection",
        None,
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let body = body_json(resp).await;
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("未配置"), "{msg}");
    assert!(!msg.contains("secret"), "{msg}");
}

#[tokio::test]
async fn unauthenticated_repository_status_is_401() {
    let app = test_app().await;
    let resp = common::get(&app, "/api/v1/artifact-repository").await;
    assert_eq!(resp.status(), 401);
}

//! REST 进程内集成（Spec B2a 测试缝）：经与二进制相同的 Router 组合根做
//! oneshot 请求——不起 socket、不 spawn 进程。只测外部行为：HTTP 状态码
//! 与 JSON 形态。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use sisyphus_server::api::router;
use tower::ServiceExt;

/// 进程内 GET（每个用例现装组合根，互不共享状态）。
async fn get(path: &str) -> Response {
    router()
        .oneshot(Request::get(path).body(Body::empty()).expect("构造请求"))
        .await
        .expect("oneshot")
}

/// 读出响应体为 UTF-8 文本。
async fn body_text(resp: Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("UTF-8")
}

#[tokio::test]
async fn healthz_returns_200_without_any_dependency() {
    // healthz 不鉴权、不查库（ADR-0010/0019）：Router 组合根本就没有
    // 存储依赖，能答 200 即证进程存活语义。
    let resp = get("/healthz").await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .expect("带 Content-Type")
            .to_str()
            .expect("ASCII")
            .starts_with("application/json")
    );

    let body: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("JSON 体");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn api_unknown_path_returns_unified_json_404() {
    // /api 前缀未命中：统一 JSON 错误形态（错误码 + message），供客户端稳定解析。
    let resp = get("/api/v1/does-not-exist").await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .expect("带 Content-Type")
            .to_str()
            .expect("ASCII")
            .starts_with("application/json")
    );

    let body: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("JSON 体");
    assert_eq!(body["code"], "NOT_FOUND");
    assert!(
        body["message"].as_str().is_some_and(|m| !m.is_empty()),
        "message 非空：{body}"
    );
}

#[tokio::test]
async fn non_api_unknown_path_keeps_plain_404() {
    // 非 /api 未命中维持普通 404（SPA fallback 归 B2a-T5，届时改语义）。
    let resp = get("/some-frontend-path").await;

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        !resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json")),
        "普通 404 不落 JSON 错误形态"
    );
    assert_eq!(body_text(resp).await, "");
}

/// Swagger UI 与 OpenAPI JSON 仅开发期（debug 构建）挂载（ADR-0005）。
#[cfg(debug_assertions)]
#[tokio::test]
async fn swagger_ui_browsable_and_openapi_json_fetchable() {
    use sisyphus_server::api::ApiDoc;
    use utoipa::OpenApi;

    // UI：/swagger-ui 重定向到带斜杠的入口，入口本身 200 且是 HTML。
    let entry = get("/swagger-ui").await;
    assert!(
        entry.status().is_redirection() || entry.status() == StatusCode::OK,
        "swagger-ui 入口可达：{}",
        entry.status()
    );

    let page = get("/swagger-ui/").await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html")),
        "swagger-ui 返回 HTML"
    );

    // OpenAPI JSON：可获取，且与组合根用的同一份 ApiDoc 完全一致。
    let resp = get("/api-docs/openapi.json").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let served: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("OpenAPI JSON");
    let expect: serde_json::Value = serde_json::from_str(
        &ApiDoc::openapi().to_json().expect("生成 OpenAPI JSON"),
    )
    .expect("OpenAPI JSON");
    assert_eq!(served, expect, "HTTP 面与 ApiDoc 逐字一致");
}

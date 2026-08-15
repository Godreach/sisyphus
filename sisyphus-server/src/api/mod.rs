//! REST API 组合根（ADR-0005/0010；票 B2a-T3）。
//!
//! - 业务端点全部挂 `/api/v1/` 前缀（B2a-T4 起接入），统一 JSON 错误形态
//!   （[`error`]）供后续端点直接复用。
//! - `GET /healthz` 不鉴权、不查库，仅表进程存活（Docker HEALTHCHECK 探活，
//!   ADR-0010/0019）。
//! - Swagger UI 与 OpenAPI JSON 仅开发期（debug 构建）挂载。
//! - 非 `/api` 未命中路径维持普通 404；SPA fallback 归 B2a-T5。

pub mod docs;
pub mod error;
pub mod health;

use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

pub use docs::ApiDoc;

use crate::api::error::ApiError;

/// REST Router 组合根：与二进制 main 相同的装配，测试经
/// `tower::ServiceExt::oneshot` 进程内驱动（Spec B2a：不起 socket、
/// 不 spawn 进程）。
pub fn router() -> Router {
    // /api/v1/ 业务端点（B2a-T4 起挂 projects/pipelines 等）：
    // 层内未命中统一走 JSON 404。
    let v1 = Router::new().fallback(api_not_found);

    let app = Router::new()
        .route("/healthz", get(health::healthz))
        .nest("/api/v1", v1)
        .fallback(fallback);

    // 开发期文档路由（Spec B2a §3：Swagger UI 挂载仅开发期；ADR-0005 只定
    // 「挂进路由」，debug 构建暴露、release 不暴露）。
    #[cfg(debug_assertions)]
    let app = {
        use utoipa::OpenApi as _;
        app.merge(
            utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
                .url("/api-docs/openapi.json", ApiDoc::openapi()),
        )
    };

    app
}

/// 根层未命中兜底：`/api` 前缀回统一 JSON 404（客户端可稳定解析错误形态）；
/// 其余维持普通 404（SPA fallback 归 B2a-T5）。
async fn fallback(uri: Uri) -> Response {
    if uri.path().starts_with("/api") {
        api_not_found().await
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// `/api/v1` 层内未命中：统一 JSON 404。
async fn api_not_found() -> Response {
    ApiError::not_found().into_response()
}

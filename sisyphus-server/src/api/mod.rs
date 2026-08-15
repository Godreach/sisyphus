//! REST API 组合根（ADR-0005/0010；票 B2a-T3/T4）。
//!
//! - 业务端点全部挂 `/api/v1/` 前缀，统一 JSON 错误形态（[`error`]）。
//! - 存储依赖经 [`AppState`] 注入（池 → repo → handler，Spec B2a §6 组合根），
//!   测试与二进制共用同一装配。
//! - `GET /healthz` 不鉴权、不查库，仅表进程存活（Docker HEALTHCHECK 探活，
//!   ADR-0010/0019）。
//! - Swagger UI 与 OpenAPI JSON 仅开发期（debug 构建）挂载。
//! - 非 `/api` 未命中路径走静态资源解析（web 模块：本地覆盖目录 → 内嵌
//!   sisyphus-web 产物 → SPA fallback 回 index.html，B2a-T5）。

pub mod docs;
pub mod error;
pub mod health;
pub mod pipelines;
pub mod projects;

mod web;

use std::path::PathBuf;

use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sqlx::SqlitePool;

pub use docs::ApiDoc;

use crate::api::error::ApiError;
use crate::store::pipelines::PipelineRepo;
use crate::store::projects::ProjectRepo;

/// REST 层共享状态：repo 组合注入（池只在 [`AppState::new`] 处消费一次）。
#[derive(Debug, Clone)]
pub struct AppState {
    /// 项目元数据 repo。
    pub projects: ProjectRepo,
    /// pipeline 定义 repo。
    pub pipelines: PipelineRepo,
}

impl AppState {
    /// 由连接池装配（组合根：开池+迁移在 [`crate::store::bootstrap`]）。
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            projects: ProjectRepo::new(pool.clone()),
            pipelines: PipelineRepo::new(pool),
        }
    }
}

/// REST Router 组合根：与二进制 main 相同的装配，测试经
/// `tower::ServiceExt::oneshot` 进程内驱动（Spec B2a：不起 socket、
/// 不 spawn 进程）。`web_override_dir` 是静态资源本地覆盖目录
/// （数据目录 `web/` 子目录，B2a-T5），不存在即纯内嵌。
pub fn router(state: AppState, web_override_dir: PathBuf) -> Router {
    // /api/v1/ 业务端点：层内未命中统一走 JSON 404。
    let v1 = Router::new()
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/{name}", get(projects::get_one))
        .route(
            "/projects/{name}/pipelines/{pipeline}",
            get(pipelines::get_definition).put(pipelines::put_definition),
        )
        .fallback(api_not_found)
        .with_state(state);

    let app = Router::new()
        .route("/healthz", get(health::healthz))
        .nest("/api/v1", v1)
        .fallback(fallback)
        .with_state(web_override_dir);

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

/// 根层未命中兜底：`/api` 前缀回统一 JSON 404（客户端可稳定解析错误形态，
/// 不落 SPA fallback）；其余走静态资源解析（B2a-T5：覆盖目录 → 内嵌 →
/// index.html fallback）。
async fn fallback(State(web_override_dir): State<PathBuf>, uri: Uri) -> Response {
    if uri.path().starts_with("/api") {
        api_not_found().await
    } else {
        web::serve(&web_override_dir, uri.path())
    }
}

/// `/api/v1` 层内未命中：统一 JSON 404。
async fn api_not_found() -> Response {
    ApiError::not_found().into_response()
}

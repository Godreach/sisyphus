//! REST API 组合根（ADR-0005/0010；票 B2a-T3/T4、B2b-T1/T2/T3/T4/T5）。
//!
//! - 业务端点全部挂 `/api/v1/` 前缀，统一 JSON 错误形态（[`error`]）。
//! - `/api/v1` 受保护段全局面挂两层中间件：认证（[`auth::require_auth`]，
//!   401，票 B2b-T1/T3：cookie 会话与 Bearer PAT 双通道）在外层先跑；
//!   CSRF 防护（[`csrf::csrf_protect`]，403，票 B2b-T2）在其内层——只拦
//!   「已认证且以 cookie 认证」的非安全方法请求（Bearer 天然免疫）。
//!   授权（403/404）不做全局中间件：角色是「项目 × 用户」函数，由各端点
//!   声明 [`policy`] 的 extractor 裁决（项目域票 B2b-T5；全局 admin 域票
//!   B2b-T4；矩阵本体在 [`crate::auth`]）。放行清单仅 login、register
//!   （开关限定，票 B2b-T4）、setup（healthz 与静态资源面不在 `/api/v1`
//!   下，天然不拦）。未匹配路由不走中间件，维持 JSON 404 兜底。
//! - 存储依赖经 [`AppState`] 注入（池 → repo → handler，Spec B2a §6 组合根），
//!   测试与二进制共用同一装配；登录限流器为进程内状态，随 [`AppState`]
//!   存活（重启即清，票 B2b-T2）；注册开关（config `[auth]
//!   registration_enabled`，默认关）随 [`AppState`] 注入 register 端点
//!   （票 B2b-T4）。
//! - `GET /healthz` 不鉴权、不查库，仅表进程存活（Docker HEALTHCHECK 探活，
//!   ADR-0010/0019）。
//! - Swagger UI 与 OpenAPI JSON 仅开发期（debug 构建）挂载。
//! - 非 `/api` 未命中路径走静态资源解析（web 模块：本地覆盖目录 → 内嵌
//!   sisyphus-web 产物 → SPA fallback 回 index.html，B2a-T5）。

pub mod auth;
pub mod csrf;
pub mod docs;
pub mod error;
pub mod health;
pub mod members;
pub mod pipelines;
pub mod policy;
pub mod projects;
pub mod tokens;
pub mod users;

mod web;

use std::path::PathBuf;

use axum::Router;
use axum::extract::State;
use axum::http::Uri;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use sqlx::SqlitePool;

pub use docs::ApiDoc;

use crate::api::error::ApiError;
use crate::auth::LoginRateLimiter;
use crate::store::members::MemberRepo;
use crate::store::pipelines::PipelineRepo;
use crate::store::projects::ProjectRepo;
use crate::store::sessions::SessionRepo;
use crate::store::tokens::PatRepo;
use crate::store::users::UserRepo;

/// REST 层共享状态：repo 组合注入（池只在 [`AppState::new`] 处消费一次）。
#[derive(Debug, Clone)]
pub struct AppState {
    /// 项目元数据 repo。
    pub projects: ProjectRepo,
    /// pipeline 定义 repo。
    pub pipelines: PipelineRepo,
    /// 用户 repo（认证面）。
    pub users: UserRepo,
    /// 会话 repo（认证面）。
    pub sessions: SessionRepo,
    /// PAT repo（认证面 + 管理端点，票 B2b-T3）。
    pub pats: PatRepo,
    /// 项目成员 repo（授权面 + 成员管理端点，票 B2b-T5）。
    pub members: MemberRepo,
    /// 登录限流器（进程内状态：per-IP / per-username 双键，重启即清，
    /// 票 B2b-T2）。
    pub login_limiter: LoginRateLimiter,
    /// 用户自注册开关（config `[auth] registration_enabled`，默认关；
    /// register 端点的门，票 B2b-T4）。
    pub registration_enabled: bool,
}

impl AppState {
    /// 由连接池装配（组合根：开池+迁移在 [`crate::store::bootstrap`]）。
    /// `registration_enabled` 来自合并后的启动配置（CLI > env > toml >
    /// 默认，ADR-0010）。
    pub fn new(pool: SqlitePool, registration_enabled: bool) -> Self {
        Self {
            projects: ProjectRepo::new(pool.clone()),
            pipelines: PipelineRepo::new(pool.clone()),
            users: UserRepo::new(pool.clone()),
            sessions: SessionRepo::new(pool.clone()),
            pats: PatRepo::new(pool.clone()),
            members: MemberRepo::new(pool),
            login_limiter: LoginRateLimiter::new(),
            registration_enabled,
        }
    }
}

/// REST Router 组合根：与二进制 main 相同的装配，测试经
/// `tower::ServiceExt::oneshot` 进程内驱动（Spec B2a：不起 socket、
/// 不 spawn 进程）。`web_override_dir` 是静态资源本地覆盖目录
/// （数据目录 `web/` 子目录，B2a-T5），不存在即纯内嵌。
pub fn router(state: AppState, web_override_dir: PathBuf) -> Router {
    // /api/v1/ 业务端点。放行清单即路由结构：login/register/setup 挂公开
    // 段（不设认证中间件；register 的开关限定与 setup 的空库限定由 handler
    // 裁决），其余全部过认证中间件（401 全局面）。层内未命中统一走 JSON
    // 404（route_layer 不罩 fallback，未匹配路由维持 404 形态不因认证状态
    // 改变）。
    let v1_public = Router::new()
        .route("/auth/setup", post(auth::setup))
        .route("/auth/login", post(auth::login))
        .route("/auth/register", post(auth::register));

    let v1_protected = Router::new()
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/password", post(auth::change_password))
        .route("/auth/tokens", get(tokens::list).post(tokens::create))
        .route("/auth/tokens/{id}", delete(tokens::revoke))
        .route("/users", get(users::list).post(users::create))
        .route("/users/directory", get(users::directory))
        .route("/users/{name}", patch(users::patch))
        .route("/users/{name}/password", put(users::reset_password))
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/{name}", get(projects::get_one))
        .route(
            "/projects/{name}/members",
            get(members::list).put(members::replace),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}",
            get(pipelines::get_definition).put(pipelines::put_definition),
        )
        // 层序（route_layer 后加者在外、先跑）：认证（401）在外层把关
        // 「谁在说话」（cookie 会话 / Bearer PAT 双通道，票 B2b-T3）；
        // CSRF（403）在其内层，只拦「已过认证且以 cookie 认证」的非安全
        // 方法请求——Bearer 面天然免疫（票 B2b-T2）。
        .route_layer(from_fn(csrf::csrf_protect))
        .route_layer(from_fn_with_state(state.clone(), auth::require_auth));

    let v1 = v1_public
        .merge(v1_protected)
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

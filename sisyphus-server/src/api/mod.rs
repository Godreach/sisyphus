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

pub mod agents;
pub mod audit;
pub mod auth;
pub mod builds;
pub mod csrf;
pub mod docs;
pub mod error;
pub mod health;
pub mod logs;
pub mod members;
pub mod pipelines;
pub mod policy;
pub mod projects;
pub mod secrets;
pub mod tokens;
pub mod triggers;
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
use crate::engine::Engine;
use crate::events::EventBus;
use crate::secrets::MasterKey;
use crate::store::SqliteLogStore;
use crate::store::agents::AgentRepo;
use crate::store::audit::AuditRepo;
use crate::store::members::MemberRepo;
use crate::store::pipelines::PipelineRepo;
use crate::store::projects::ProjectRepo;
use crate::store::secrets::SecretRepo;
use crate::store::sessions::SessionRepo;
use crate::store::tokens::PatRepo;
use crate::store::triggers::TriggerRepo;
use crate::store::users::UserRepo;

/// REST 层共享状态：repo 组合注入（池在 [`AppState::new`] 处消费一次后
/// 随状态存留——repo 共池，句柄克隆零成本）。
#[derive(Debug, Clone)]
pub struct AppState {
    /// 底层连接池（组合根持有；repo 共池，供跨 repo 复用——如 Agent
    /// 详情在途任务数经 jobs repo 计数）。
    pub pool: SqlitePool,
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
    /// 项目机密 repo（票 B2b-T6：建/覆写/列名/删，值只写不读）。
    pub secrets: SecretRepo,
    /// Agent 注册面 repo（票 B2c-T3：建条目/启停/编辑/在线维护/标签匹配；
    /// REST 面与 gRPC 通道认证面共用）。
    pub agents: AgentRepo,
    /// 触发器 repo（票 B2c-T6，ADR-0016：cron/poll 触发源 CRUD + 基线/探测
    /// 历史；REST 面 CRUD 消费，trigger 引擎后台扫表消费）。
    pub triggers: TriggerRepo,
    /// 审计日志 repo（票 B2b-T7，ADR-0015：只增 + 过滤回放，全局 admin
    /// 查询端点消费；各端点安全事件接线处写入）。
    pub audit: AuditRepo,
    /// 构建日志存储（票 #73，ADR-0013）：grpc 落库（写）与 SSE 回放/下载
    /// （读，独立连接）两消费面。
    pub logs: SqliteLogStore,
    /// 编排引擎（票 B2c-T2，ADR-0006：统一触发入口 + 构建推进 + 任务终态
    /// 接线点；sched/grpc/REST 共享同一引擎与事件总线）。
    pub engine: Engine,
    /// 进程内事件总线（热通知，可丢，DB 重放兜底；sched 循环消费）。
    pub bus: EventBus,
    /// 主密钥（票 B2b-T6，ADR-0015）：首启由启动路径经
    /// [`crate::secrets::ensure_master_key`] 生成/读回后注入，机密写入路径
    /// 加密用。
    pub master_key: MasterKey,
    /// 登录限流器（进程内状态：per-IP / per-username 双键，重启即清，
    /// 票 B2b-T2）。
    pub login_limiter: LoginRateLimiter,
    /// 用户自注册开关（config `[auth] registration_enabled`，默认关；
    /// register 端点的门，票 B2b-T4）。
    pub registration_enabled: bool,
    /// poll 触发器轮询节奏默认分钟（config `[triggers] poll_interval_minutes`，
    /// 默认 5，ADR-0016）：新建 poll 触发器未显式给节奏时取此值，进触发器
    /// spec（票 B2c-T6）。
    pub poll_interval_minutes: i64,
}

impl AppState {
    /// 由连接池装配（组合根：开池+迁移在 [`crate::store::bootstrap`]）。
    /// `registration_enabled` 来自合并后的启动配置（CLI > env > toml >
    /// 默认，ADR-0010）；`master_key` 为机密加密主密钥（ADR-0015，票
    /// B2b-T6：启动路径已生成/读回密钥文件）；`poll_interval_minutes` 为
    /// poll 触发器节奏默认（ADR-0016，票 B2c-T6）。日志存储另开独立读
    /// 连接（ADR-0004：读独立于 gRPC 写路径），开池失败折组合根装配失败。
    pub async fn new(
        pool: SqlitePool,
        registration_enabled: bool,
        master_key: MasterKey,
        poll_interval_minutes: i64,
    ) -> Result<Self, crate::store::StoreError> {
        let bus = EventBus::new();
        Ok(Self {
            pool: pool.clone(),
            projects: ProjectRepo::new(pool.clone()),
            pipelines: PipelineRepo::new(pool.clone()),
            users: UserRepo::new(pool.clone()),
            sessions: SessionRepo::new(pool.clone()),
            pats: PatRepo::new(pool.clone()),
            members: MemberRepo::new(pool.clone()),
            secrets: SecretRepo::new(pool.clone()),
            agents: AgentRepo::new(pool.clone()),
            triggers: TriggerRepo::new(pool.clone()),
            audit: AuditRepo::new(pool.clone()),
            logs: SqliteLogStore::open(&pool).await?,
            engine: Engine::new(pool.clone(), master_key, bus.clone()),
            bus,
            master_key,
            login_limiter: LoginRateLimiter::new(),
            registration_enabled,
            poll_interval_minutes,
        })
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
        .route("/auth/register", post(auth::register))
        // 注册码兑 token（票 #57）：Agent 凭注册码换长期 token，此刻没有
        // token 只有注册码——公开端点（与 login 同档放行；注册码本身即
        // 高熵随机，哈希匹配即身份）。
        .route("/agent/register", post(agents::register));

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
        .route("/projects/{name}/secrets", get(secrets::list_secrets))
        .route(
            "/projects/{name}/secrets/{secret}",
            put(secrets::put_secret).delete(secrets::delete_secret),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}",
            get(pipelines::get_definition).put(pipelines::put_definition),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds",
            get(builds::list).post(builds::trigger),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds/{number}",
            get(builds::detail),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds/{number}/cancel",
            post(builds::cancel),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds/{number}/rerun",
            post(builds::rerun),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds/{number}/jobs/{job}/attempts/{attempt}/logs",
            get(logs::download),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/builds/{number}/jobs/{job}/attempts/{attempt}/logs/stream",
            get(logs::stream),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/triggers",
            get(triggers::list).post(triggers::create),
        )
        .route(
            "/projects/{name}/pipelines/{pipeline}/triggers/{kind}",
            get(triggers::get_one).patch(triggers::patch),
        )
        .route("/audit", get(audit::list))
        .route("/agents", get(agents::list).post(agents::create))
        .route("/agents/{name}", get(agents::get_one).patch(agents::patch))
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

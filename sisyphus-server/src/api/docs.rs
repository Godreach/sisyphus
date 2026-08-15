//! OpenAPI 契约（utoipa 5，ADR-0005）：注解即契约，snapshot 守护防漂移
//! （`sisyphus-server/tests/openapi_snapshot.rs`）。

use utoipa::OpenApi;

use super::error::ErrorBody;
use super::health;

/// Server REST API 契约。
///
/// Swagger UI 开发期（debug 构建）浏览：`/swagger-ui`；OpenAPI JSON：
/// `/api-docs/openapi.json`。
#[derive(OpenApi)]
#[openapi(
    info(
        title = "sisyphus Server API",
        version = env!("CARGO_PKG_VERSION"),
        description = "自托管 CI 平台 Server REST API。业务端点均在 /api/v1/ 前缀下；\
                       B2a 阶段暂无鉴权（auth 批次统一中间件补）。",
    ),
    paths(health::healthz),
    components(schemas(health::Healthz, ErrorBody)),
    tags(
        (name = "infra", description = "探针与基础设施端点"),
    )
)]
pub struct ApiDoc;

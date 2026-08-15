//! OpenAPI 契约（utoipa 5，ADR-0005）：注解即契约，snapshot 守护防漂移
//! （`sisyphus-server/tests/openapi_snapshot.rs`）。

use utoipa::OpenApi;

use super::error::{ErrorBody, ValidationIssue};
use super::health;
use super::pipelines;
use super::projects;

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
    paths(
        health::healthz,
        projects::list,
        projects::create,
        projects::get_one,
        pipelines::get_definition,
        pipelines::put_definition,
    ),
    components(schemas(
        health::Healthz,
        ErrorBody,
        ValidationIssue,
        projects::ScmTypeDto,
        projects::CreateProjectRequest,
        projects::ProjectResponse,
        pipelines::PipelineDefinitionPayload,
        pipelines::PipelineDefinitionResponse,
        pipelines::SaveDefinitionResponse,
    )),
    tags(
        (name = "infra", description = "探针与基础设施端点"),
        (name = "projects", description = "项目管理（v1：list / create / get）"),
        (name = "pipelines", description = "Pipeline 定义读写（model 校验 + revision 语义）"),
    )
)]
pub struct ApiDoc;

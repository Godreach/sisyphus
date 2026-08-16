//! OpenAPI 契约（utoipa 5，ADR-0005）：注解即契约，snapshot 守护防漂移
//! （`sisyphus-server/tests/openapi_snapshot.rs`）。

use utoipa::OpenApi;

use super::auth;
use super::error::{ErrorBody, ValidationIssue};
use super::health;
use super::members;
use super::pipelines;
use super::projects;
use super::tokens;
use super::users;

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
                       B2b 起会话认证全局把关（未认证一律 401，放行面仅 login/setup\
                       /healthz 与静态资源，票 B2b-T1）。",
    ),
    paths(
        health::healthz,
        auth::setup,
        auth::login,
        auth::logout,
        auth::me,
        tokens::list,
        tokens::create,
        tokens::revoke,
        projects::list,
        projects::create,
        projects::get_one,
        members::list,
        members::replace,
        pipelines::get_definition,
        pipelines::put_definition,
        users::directory,
    ),
    components(schemas(
        health::Healthz,
        ErrorBody,
        ValidationIssue,
        auth::CredentialsRequest,
        auth::MeResponse,
        tokens::CreateTokenRequest,
        tokens::TokenResponse,
        tokens::CreatedTokenResponse,
        projects::ScmTypeDto,
        projects::CreateProjectRequest,
        projects::ProjectResponse,
        members::RoleDto,
        members::MemberAssignment,
        members::MemberResponse,
        pipelines::PipelineDefinitionPayload,
        pipelines::PipelineDefinitionResponse,
        pipelines::SaveDefinitionResponse,
        users::DirectoryEntryResponse,
    )),
    tags(
        (name = "infra", description = "探针与基础设施端点"),
        (name = "auth", description = "认证与会话（setup wizard / login / logout / me）与 PAT（Bearer 通道，票 B2b-T3）"),
        (name = "projects", description = "项目管理（v1：list / create / get；成员三档角色分配，票 B2b-T5）"),
        (name = "pipelines", description = "Pipeline 定义读写（model 校验 + revision 语义）"),
        (name = "users", description = "用户目录（项目 admin 的最小只读清单，票 B2b-T5）"),
    )
)]
pub struct ApiDoc;

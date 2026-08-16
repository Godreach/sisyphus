//! OpenAPI 契约（utoipa 5，ADR-0005）：注解即契约，snapshot 守护防漂移
//! （`sisyphus-server/tests/openapi_snapshot.rs`）。

use utoipa::OpenApi;

use super::audit;
use super::auth;
use super::error::{ErrorBody, ValidationIssue};
use super::health;
use super::members;
use super::pipelines;
use super::projects;
use super::secrets;
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
        auth::register,
        auth::logout,
        auth::me,
        auth::change_password,
        tokens::list,
        tokens::create,
        tokens::revoke,
        projects::list,
        projects::create,
        projects::get_one,
        members::list,
        members::replace,
        secrets::list_secrets,
        secrets::put_secret,
        secrets::delete_secret,
        pipelines::get_definition,
        pipelines::put_definition,
        users::list,
        users::create,
        users::patch,
        users::reset_password,
        users::directory,
        audit::list,
    ),
    components(schemas(
        health::Healthz,
        ErrorBody,
        ValidationIssue,
        auth::CredentialsRequest,
        auth::MeResponse,
        auth::ChangePasswordRequest,
        tokens::CreateTokenRequest,
        tokens::TokenResponse,
        tokens::CreatedTokenResponse,
        projects::ScmTypeDto,
        projects::CreateProjectRequest,
        projects::ProjectResponse,
        members::RoleDto,
        members::MemberAssignment,
        members::MemberResponse,
        secrets::PutSecretRequest,
        secrets::SecretNameResponse,
        pipelines::PipelineDefinitionPayload,
        pipelines::PipelineDefinitionResponse,
        pipelines::SaveDefinitionResponse,
        users::UserResponse,
        users::CreateUserRequest,
        users::PatchUserRequest,
        users::ResetPasswordRequest,
        users::DirectoryEntryResponse,
        audit::AuditEntryResponse,
    )),
    tags(
        (name = "infra", description = "探针与基础设施端点"),
        (name = "auth", description = "认证与会话（setup wizard / register / login / logout / me / 自助改密）与 PAT（Bearer 通道，票 B2b-T3）"),
        (name = "projects", description = "项目管理（v1：list / create / get；成员三档角色分配，票 B2b-T5）"),
        (name = "secrets", description = "项目机密（票 B2b-T6：值只写不读——建/覆写、仅名清单、删；viewer/runner 连名不可见，项目 admin 档）"),
        (name = "pipelines", description = "Pipeline 定义读写（model 校验 + revision 语义）"),
        (name = "users", description = "用户管理（全局 admin：建/列/禁用/重置，票 B2b-T4）与用户目录（项目 admin 的最小只读清单，票 B2b-T5）"),
        (name = "audit", description = "审计回放（票 B2b-T7：安全事件只增记账，仅全局 admin 可查询——按时间/用户/项目/事件类型过滤 + 分页）"),
    )
)]
pub struct ApiDoc;

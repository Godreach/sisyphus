//! 产物 REST 端点（票 #74 / B5-T2，ADR-0004/0006/0007/0008）。
//!
//! 两个认证面（ADR-0007：产物走 HTTP 不走 gRPC 流）：
//!
//! - **Agent 面**（[`require_agent_auth`] 中间件，`/api/v1/agent/artifacts/…`）：
//!   `Authorization: Bearer sisa_…`（Agent token 族，与 gRPC 通道同一查行面
//!   [`AgentRepo::find_active_by_hash`]；非 Agent token 一律 401——PAT/会话
//!   不混入本面）。
//!   - 上传 `POST /agent/artifacts/{job_id}/{name}`：请求体即产物字节流式
//!     写盘（不整读入内存）、边写边算 SHA-256，完成后记元数据行。`job_id`
//!     为上传任务自身行 id（JobSpec.job_id 同源）——Server 侧解析出
//!     build_id 定位磁盘目录。
//!   - 下载依赖 `GET /agent/artifacts/{job_id}/downloads/{source_job}/{name}`：
//!     `job_id` 为拉取任务自身行 id（由此定位构建）、`source_job` 为声明里
//!     的来源任务名（报错定位用）、`name` 为产物名。产物按 (build, name)
//!     寻址——**尚不存在**（来源任务未成功上传）时 404 附清晰报错，Agent
//!     侧据此任务失败（不静默等待）。
//! - **用户面**（viewer 档，挂构建资源下）：构建产物列表（详情页产物区数据
//!   源）+ 单产物流式下载（响应头带 `Content-Length`（大小）与
//!   `X-Sisyphus-Sha256`（校验和））。
//!
//! 槽位语义（ADR-0008）：槽位占用到**产物上传完成**由时序保证——Agent 在
//! 步骤全部成功、缓存 save 之后、终态上报之前上传产物，Server 侧终态
//! （含 JobAck/JobStatus 的槽位释放判定）只认终态上报，故上传中任务不
//! 释放槽位。上传失败 Agent 上报任务失败（非静默）。

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::bearer_token;
use super::builds::load_build;
use super::error::{ApiError, ErrorBody, ValidationIssue};
use super::policy::RequireViewer;
use crate::auth::{TokenFamily, token_family, token_hash};
use crate::store::jobs::JobRepo;
use crate::store::{ArtifactMetaRepo, ArtifactStore};

/// Agent 面认证通过的上下文（中间件注入请求扩展）。
#[derive(Debug, Clone)]
pub struct AgentAuth {
    /// Agent 行 id（产物面归属校验：任务行 `agent_id` 须是本 Agent）。
    pub agent_id: i64,
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// Agent 上传完成响应：落定的产物元数据（大小 + 校验和回执）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactUploadedResponse {
    /// 产物名。
    pub name: String,
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
}

/// 产物条目（构建产物列表）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactDto {
    /// 产物名（任务级声明的上传名）。
    pub name: String,
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
    /// 上传时刻（Unix 毫秒；重跑同名再传刷新）。
    pub created_at: i64,
}

/// 构建产物列表响应。
#[derive(Debug, Serialize, ToSchema)]
pub struct BuildArtifactsResponse {
    /// 构建全部产物（按名排序）。
    pub items: Vec<ArtifactDto>,
}

// ---------------------------------------------------------------------------
// Agent 面认证中间件
// ---------------------------------------------------------------------------

/// Agent token 认证中间件（产物 Agent 面）：Bearer `sisa_…` → 哈希查
/// agents 表（未停用）→ 注入 [`AgentAuth`]；缺失/非 Agent 族/停用/查无
/// 一律 401 统一 JSON 形态。PAT（`sis_`）与 cookie 会话是用户面凭据，
/// 在本面恒 401（两族不混用，ADR-0014）。
pub async fn require_agent_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // 仅 Bearer 面（Agent 无 cookie 语义）；其它 scheme / 缺头 401。
    let Some(token) = bearer_token(req.headers()) else {
        return ApiError::unauthorized().into_response();
    };
    if token_family(&token) != Some(TokenFamily::Agent) {
        return ApiError::unauthorized().into_response();
    }
    let hash = token_hash(&token);
    let agent = match state.agents.find_active_by_hash(&hash).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return ApiError::unauthorized().into_response(),
        Err(e) => return ApiError::internal("agent token lookup", &e).into_response(),
    };
    req.extensions_mut().insert(AgentAuth {
        agent_id: agent.id,
    });
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Agent 面端点
// ---------------------------------------------------------------------------

/// Agent 产物上传（agent token 鉴权，票 #74 / ADR-0007）：请求体即产物
/// 字节（流式写盘，不整读内存），落定后记元数据行，返回大小 + 校验和。
#[utoipa::path(
    post,
    path = "/api/v1/agent/artifacts/{job_id}/{name}",
    tag = "artifacts",
    request_body(content = Vec<u8>, content_type = "application/octet-stream",
        description = "产物字节流（chunked/流式，服务端不设 v1 体积上限）"),
    params(
        ("job_id" = i64, Path, description = "上传任务自身行 id（JobSpec.job_id 同源）"),
        ("name" = String, Path, description = "产物名（任务级声明；不得含路径分隔符）"),
    ),
    responses(
        (status = 201, description = "已落盘并记元数据", body = ArtifactUploadedResponse),
        (status = 401, description = "未认证（仅 Agent token `sisa_` 族可用；PAT/会话 401）", body = ErrorBody),
        (status = 404, description = "任务行不存在", body = ErrorBody),
        (status = 422, description = "产物名非法（空/含路径分隔符/超长）", body = ErrorBody),
    )
)]
pub async fn agent_upload(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, name)): Path<(i64, String)>,
    body: Body,
) -> Result<(StatusCode, Json<ArtifactUploadedResponse>), ApiError> {
    validate_name(&name)?;
    let job = load_own_job(&state, &agent, job_id).await?;

    // 请求体流 → 字节流缝（axum DataStream 的错误归一为 io::Error；Bytes
    // → Vec 与缝的元素型对齐）。
    let stream = body
        .into_data_stream()
        .map(|r| r.map(|b| b.to_vec()).map_err(std::io::Error::other))
        .boxed();
    let meta = state
        .artifacts
        .store(job.build_id, &name, stream)
        .await
        .map_err(|e| ApiError::internal("产物落盘", &e))?;
    state
        .artifact_meta
        .record(&meta)
        .await
        .map_err(|e| ApiError::internal("产物元数据落库", &e))?;
    Ok((
        StatusCode::CREATED,
        Json(ArtifactUploadedResponse {
            name: meta.name,
            size: meta.size,
            sha256: meta.sha256,
        }),
    ))
}

/// Agent 依赖产物下载（agent token 鉴权，票 #74）：拉取本次构建内其它
/// 任务的产物。`job_id` 定位构建，`source_job`/`name` 定位产物（声明的
/// 来源任务名用于报错定位）。产物尚不存在 → 404 附清晰报错。
#[utoipa::path(
    get,
    path = "/api/v1/agent/artifacts/{job_id}/downloads/{source_job}/{name}",
    tag = "artifacts",
    params(
        ("job_id" = i64, Path, description = "拉取任务自身行 id（由此定位构建）"),
        ("source_job" = String, Path, description = "声明的来源任务名（报错定位用）"),
        ("name" = String, Path, description = "产物名"),
    ),
    responses(
        (status = 200, description = "产物字节流（响应头 Content-Length + X-Sisyphus-Sha256）", content_type = "application/octet-stream"),
        (status = 401, description = "未认证（仅 Agent token `sisa_` 族可用）", body = ErrorBody),
        (status = 404, description = "任务不存在 / 来源任务不存在 / 依赖产物尚不存在（未上传）", body = ErrorBody),
    )
)]
pub async fn agent_download(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, source_job, name)): Path<(i64, String, String)>,
) -> Result<Response, ApiError> {
    let job = load_own_job(&state, &agent, job_id).await?;
    let build_id = job.build_id;

    // 来源任务名校验（清晰报错定位：声明错名与产物未上传是两种失败）。
    let jobs = JobRepo::new(state.pool.clone())
        .list_by_build(build_id)
        .await?;
    if !jobs.iter().any(|j| j.name == source_job) {
        return Err(ApiError::resource_not_found(format!(
            "来源任务 {source_job} 在本次构建内不存在"
        )));
    }

    let meta = state
        .artifact_meta
        .find(build_id, &name)
        .await?
        .ok_or_else(|| {
            // 「依赖产物尚不存在」的清晰报错（票 #74 AC）：Agent 侧据此
            // 任务失败，不静默空等。
            ApiError::resource_not_found(format!(
                "依赖产物尚不存在：任务 {source_job} 的产物 {name} 未上传"
            ))
        })?;
    artifact_response(&state, meta.sha256.clone(), meta.size, build_id, &name).await
}

// ---------------------------------------------------------------------------
// 用户面端点（viewer 档）
// ---------------------------------------------------------------------------

/// 构建产物列表（viewer 档，票 #74）：构建详情页产物区数据源。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/artifacts",
    tag = "artifacts",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
    ),
    responses(
        (status = 200, description = "构建全部产物（按名排序）", body = BuildArtifactsResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或构建号不存在", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number)): Path<(String, String, i64)>,
) -> Result<Json<BuildArtifactsResponse>, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let entries = state
        .artifact_meta
        .list_with_created_at(build.id)
        .await
        .map_err(|e| ApiError::internal("产物列表查询", &e))?;
    Ok(Json(BuildArtifactsResponse {
        items: entries
            .into_iter()
            .map(|e| ArtifactDto {
                name: e.meta.name,
                size: e.meta.size,
                sha256: e.meta.sha256,
                created_at: e.created_at,
            })
            .collect(),
    }))
}

/// 单产物下载（viewer 档，票 #74）：流式响应，响应头带大小
/// （Content-Length）与校验和（X-Sisyphus-Sha256）。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/artifacts/{artifact}",
    tag = "artifacts",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("number" = i64, Path, description = "构建号"),
        ("artifact" = String, Path, description = "产物名"),
    ),
    responses(
        (status = 200, description = "产物字节流（Content-Length = 大小，X-Sisyphus-Sha256 = 校验和）", content_type = "application/octet-stream"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，构建号或产物不存在", body = ErrorBody),
    )
)]
pub async fn download(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number, artifact)): Path<(String, String, i64, String)>,
) -> Result<Response, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let meta = state
        .artifact_meta
        .find(build.id, &artifact)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("产物 {artifact} 不存在")))?;
    artifact_response(&state, meta.sha256, meta.size, build.id, &artifact).await
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// 取任务行并校验归属：行存在且 `agent_id` 是认证 Agent（产物面只许
/// 写/读自己承接的任务——他人任务 404 同形，不泄存在性）。
async fn load_own_job(
    state: &AppState,
    agent: &AgentAuth,
    job_id: i64,
) -> Result<crate::store::jobs::JobRow, ApiError> {
    JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
        .filter(|j| j.agent_id == Some(agent.agent_id))
        .ok_or_else(|| ApiError::resource_not_found(format!("任务 {job_id} 不存在")))
}

/// 产物名校验（与 store 层同规则）：非法 422（不静默放宽）。
fn validate_name(name: &str) -> Result<(), ApiError> {
    crate::store::validate_artifact_name(name).map_err(|e| {
        ApiError::validation(
            "产物名非法",
            vec![ValidationIssue {
                path: "name".into(),
                message: e.to_string(),
            }],
        )
    })
}

/// 打开字节流并组装下载响应（Agent 面 / 用户面共用）：流式 body +
/// Content-Length（大小）+ X-Sisyphus-Sha256（校验和）+ 附件文件名。
async fn artifact_response(
    state: &AppState,
    sha256: String,
    size: u64,
    build_id: i64,
    name: &str,
) -> Result<Response, ApiError> {
    let stream = state
        .artifacts
        .open(build_id, name)
        .await
        .map_err(|e| ApiError::internal("产物读取", &e))?;
    let body = Body::from_stream(stream.map(|r| r.map(axum::body::Bytes::from)));
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_LENGTH, size.to_string().parse().expect("长度为合法头值"));
    headers.insert(
        header::HeaderName::from_static("x-sisyphus-sha256"),
        sha256.parse().expect("sha256 hex 为合法头值"),
    );
    // 文件名仅 ASCII 安全字符子集（已过名校验）；attachment 触发浏览器下载。
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{name}\"")
            .parse()
            .expect("产物名为合法头值（无控制字符/引号外字符）"),
    );
    Ok((StatusCode::OK, headers, body).into_response())
}

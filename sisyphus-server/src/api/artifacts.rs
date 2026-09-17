//! 产物 REST 端点（票 #74 / B5-T2，ADR-0004/0006/0007/0008）。
//!
//! 两个认证面（ADR-0007：产物走 HTTP 不走 gRPC 流）：
//!
//! - **Agent 面**（[`require_agent_auth`] 中间件，`/api/v1/agent/artifacts/…`）：
//!   `Authorization: Bearer sisa_…`（Agent token 族，与 gRPC 通道同一查行面
//!   [`AgentRepo::find_active_by_hash`]；非 Agent token 一律 401——PAT/会话
//!   不混入本面）。
//!   - 上传 `POST /agent/artifacts/{job_id}/{name}`：未配置 S3 时请求体即
//!     产物字节流式写盘。已配置 S3 时 409，改走直传。
//!   - 直传 `POST /agent/artifacts/{job_id}/{name}/upload-url`（票 #123）：
//!     仅任务上传声明内的名，签发临时 key 短期 PUT URL（不含长期凭据）。
//!   - 完成 `POST /agent/artifacts/{job_id}/{name}/complete`：流式 SHA-256
//!     核验临时对象，复制到从未签发写权限的最终 key 后 ready；完成前不可见。
//!   - 下载依赖 `GET /agent/artifacts/{job_id}/downloads/{source_job}/{name}`：
//!     `job_id` 为拉取任务自身行 id（由此定位构建）、`source_job` 为声明里
//!     的来源任务名（报错定位用）、`name` 为产物名。产物按 (build, name)
//!     寻址——**尚不存在**（来源任务未成功上传）时 404 附清晰报错，Agent
//!     侧据此任务失败（不静默等待）。S3 产物 302 到短期 GET URL。
//! - **用户面**（viewer 档，挂构建资源下）：构建产物列表（详情页产物区数据
//!   源）+ 本地下载字节流 / S3 短期 GET URL（302 Location）。
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
use serde::{Deserialize, Serialize};
use sisyphus_model::validate::BuildSnapshot;
use utoipa::ToSchema;

use super::AppState;
use super::auth::bearer_token;
use super::builds::load_build;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireViewer;
use crate::auth::{TokenFamily, token_family, token_hash};
use crate::storage::{ObjectClass, ObjectPhase, artifact_blob_name, object_key};
use crate::store::builds::BuildRepo;
use crate::store::jobs::JobRepo;
use crate::store::{ArtifactBackend, ArtifactMeta, ArtifactMetaRepo, ArtifactState, ArtifactStore};

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

/// Agent 预签名上传 URL（仅临时对象，票 #123）。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactUploadUrlResponse {
    /// 短期 PUT URL（查询串签名，不含长期 secret）。
    pub url: String,
    /// 有效秒数。
    pub expires_in: i64,
}

/// Agent 完成上传：声明的大小与 SHA-256。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactCompleteRequest {
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
}

/// 预签名有效期（秒）：上传 PUT 与用户 GET 同为 5 分钟。
const PRESIGN_SECS: i64 = 5 * 60;

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
    /// 正文字节所在后端（历史单文件为 `local`）。
    pub backend: ArtifactBackendDto,
    /// 上传任务行；旧数据未记录时为空。
    pub job_id: Option<i64>,
    /// 上传任务 attempt；旧数据未记录时为空。
    pub attempt: Option<i32>,
    /// 正文字节状态（`ready` / `missing` / `unavailable`；pending 不出现）。
    pub state: ArtifactStateDto,
}

/// 产物正文字节后端。
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactBackendDto {
    /// Server 本地数据目录。
    Local,
    /// S3 兼容对象存储。
    S3,
}

impl From<ArtifactBackend> for ArtifactBackendDto {
    fn from(value: ArtifactBackend) -> Self {
        match value {
            ArtifactBackend::Local => Self::Local,
            ArtifactBackend::S3 => Self::S3,
        }
    }
}

/// 产物正文可用状态。
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactStateDto {
    /// 正文可用。
    Ready,
    /// 元数据存在但正文缺失。
    Missing,
    /// S3 后端未配置，历史对象不可用。
    Unavailable,
}

impl From<ArtifactState> for ArtifactStateDto {
    fn from(value: ArtifactState) -> Self {
        match value {
            ArtifactState::Ready => Self::Ready,
            ArtifactState::Missing => Self::Missing,
            ArtifactState::Pending => Self::Missing, // 列表已过滤；防御性兜底
        }
    }
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
    req.extensions_mut()
        .insert(AgentAuth { agent_id: agent.id });
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
    if state.s3.is_some() {
        return Err(ApiError::conflict(
            "已配置 S3：请经预签名临时对象直传后再 complete",
        ));
    }
    validate_name(&name)?;
    let job = load_own_job(&state, &agent, job_id).await?;

    // 请求体流 → 字节流缝（axum DataStream 的错误归一为 io::Error；Bytes
    // → Vec 与缝的元素型对齐）。
    let stream = body
        .into_data_stream()
        .map(|r| r.map(|b| b.to_vec()).map_err(std::io::Error::other))
        .boxed();
    let mut meta = state
        .artifacts
        .store(job.build_id, &name, stream)
        .await
        .map_err(|e| ApiError::internal("产物落盘", &e))?;
    meta.job_id = Some(job.id);
    meta.attempt = Some(job.attempt);
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

/// Agent 申请单文件临时对象 PUT URL（票 #123）：仅任务上传声明内的名，
/// 不接触长期凭据；最终 key 从不签发写权限。
#[utoipa::path(
    post,
    path = "/api/v1/agent/artifacts/{job_id}/{name}/upload-url",
    tag = "artifacts",
    params(
        ("job_id" = i64, Path, description = "上传任务自身行 id"),
        ("name" = String, Path, description = "产物名（须在任务上传声明内）"),
    ),
    responses(
        (status = 200, description = "短期 PUT URL", body = ArtifactUploadUrlResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "任务行不存在", body = ErrorBody),
        (status = 409, description = "未配置 S3", body = ErrorBody),
        (status = 422, description = "产物名非法或不在上传声明内", body = ErrorBody),
    )
)]
pub async fn agent_upload_url(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, name)): Path<(i64, String)>,
) -> Result<Json<ArtifactUploadUrlResponse>, ApiError> {
    validate_name(&name)?;
    let job = load_own_job(&state, &agent, job_id).await?;
    let s3 = state
        .s3
        .as_ref()
        .ok_or_else(|| ApiError::conflict("未配置 S3，无法签发直传 URL"))?;
    ensure_declared_upload(&state, &job, &name).await?;

    let (tmp_key, final_key) = artifact_object_keys(s3.prefix(), &job, &name);
    let existing = state
        .artifact_meta
        .find_including_pending(job.build_id, &name)
        .await?;
    if existing
        .as_ref()
        .is_some_and(|m| m.state == ArtifactState::Ready)
    {
        return Err(ApiError::conflict(format!(
            "产物 {name} 已 ready，重试请 complete 同一摘要"
        )));
    }
    state
        .artifact_meta
        .record(&ArtifactMeta {
            build_id: job.build_id,
            job_id: Some(job.id),
            attempt: Some(job.attempt),
            backend: ArtifactBackend::S3,
            state: ArtifactState::Pending,
            name: name.clone(),
            path: final_key,
            size: 0,
            sha256: String::new(),
        })
        .await
        .map_err(|e| ApiError::internal("产物 pending 落库", &e))?;
    let url = s3
        .presign_put(&tmp_key, PRESIGN_SECS)
        .map_err(|e| ApiError::internal("签发上传 URL", &e))?;
    Ok(Json(ArtifactUploadUrlResponse {
        url,
        expires_in: PRESIGN_SECS,
    }))
}

/// Agent 提交清单：流式核验临时对象 SHA-256，复制到最终 key 后 ready。
#[utoipa::path(
    post,
    path = "/api/v1/agent/artifacts/{job_id}/{name}/complete",
    tag = "artifacts",
    request_body = ArtifactCompleteRequest,
    params(
        ("job_id" = i64, Path, description = "上传任务自身行 id"),
        ("name" = String, Path, description = "产物名"),
    ),
    responses(
        (status = 201, description = "已固化为 ready", body = ArtifactUploadedResponse),
        (status = 200, description = "已 ready，幂等", body = ArtifactUploadedResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "任务行不存在", body = ErrorBody),
        (status = 409, description = "未配置 S3", body = ErrorBody),
        (status = 422, description = "校验失败", body = ErrorBody),
    )
)]
pub async fn agent_complete(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, name)): Path<(i64, String)>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<ArtifactUploadedResponse>), ApiError> {
    validate_name(&name)?;
    let job = load_own_job(&state, &agent, job_id).await?;
    let s3 = state
        .s3
        .as_ref()
        .ok_or_else(|| ApiError::conflict("未配置 S3，无法完成直传"))?;
    ensure_declared_upload(&state, &job, &name).await?;
    let req: ArtifactCompleteRequest = parse_body(&body)?;
    let sha = req.sha256.to_ascii_lowercase();

    if let Some(meta) = state
        .artifact_meta
        .find_including_pending(job.build_id, &name)
        .await?
        && meta.state == ArtifactState::Ready
        && meta.backend == ArtifactBackend::S3
    {
        if meta.size == req.size && meta.sha256 == sha {
            return Ok((
                StatusCode::OK,
                Json(ArtifactUploadedResponse {
                    name: meta.name,
                    size: meta.size,
                    sha256: meta.sha256,
                }),
            ));
        }
        return Err(ApiError::conflict(format!(
            "产物 {name} 已 ready，摘要不一致"
        )));
    }

    let (tmp_key, final_key) = artifact_object_keys(s3.prefix(), &job, &name);

    let (size, digest) = match s3.hash_object(&tmp_key).await {
        Ok(v) => v,
        Err(e) => {
            return Err(ApiError::validation(
                "临时对象校验失败",
                vec![ValidationIssue {
                    path: "upload".into(),
                    message: e.to_string(),
                }],
            ));
        }
    };
    if size != req.size || digest != sha {
        let _ = s3.delete_object(&tmp_key).await;
        return Err(ApiError::validation(
            "产物内容与声明不符",
            vec![ValidationIssue {
                path: "sha256".into(),
                message: format!(
                    "声明 size={}/sha256={}，实际 size={}/sha256={digest}",
                    req.size, sha, size
                ),
            }],
        ));
    }
    s3.copy_object(&tmp_key, &final_key)
        .await
        .map_err(|e| ApiError::internal("复制最终对象", &e))?;
    let (final_size, final_digest) = match s3.hash_object(&final_key).await {
        Ok(v) => v,
        Err(e) => {
            let _ = s3.delete_object(&final_key).await;
            let _ = s3.delete_object(&tmp_key).await;
            return Err(ApiError::internal("校验最终对象", &e));
        }
    };
    if final_size != size || final_digest != digest {
        let _ = s3.delete_object(&final_key).await;
        let _ = s3.delete_object(&tmp_key).await;
        return Err(ApiError::validation(
            "最终对象校验失败",
            vec![ValidationIssue {
                path: "sha256".into(),
                message: "复制后最终对象与临时对象不一致".into(),
            }],
        ));
    }
    let _ = s3.delete_object(&tmp_key).await;
    let meta = ArtifactMeta {
        build_id: job.build_id,
        job_id: Some(job.id),
        attempt: Some(job.attempt),
        backend: ArtifactBackend::S3,
        state: ArtifactState::Ready,
        name: name.clone(),
        path: final_key,
        size,
        sha256: digest.clone(),
    };
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
        (status = 200, description = "本地产物字节流（响应头 Content-Length + X-Sisyphus-Sha256）", content_type = "application/octet-stream"),
        (status = 302, description = "S3 产物：Location 为短期 GET URL"),
        (status = 401, description = "未认证（仅 Agent token `sisa_` 族可用）", body = ErrorBody),
        (status = 404, description = "任务不存在 / 来源任务不存在 / 依赖产物尚不存在（未上传）", body = ErrorBody),
        (status = 409, description = "产物元数据存在但正文缺失或后端尚不可用", body = ErrorBody),
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
    artifact_response(&state, meta).await
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
    let mut items = Vec::with_capacity(entries.len());
    for mut entry in entries {
        if entry.meta.backend == ArtifactBackend::Local {
            let observed = state
                .artifacts
                .inspect_state(&entry.meta)
                .await
                .map_err(|e| ApiError::internal("产物状态检查", &e))?;
            if observed != entry.meta.state {
                state
                    .artifact_meta
                    .set_state(entry.meta.build_id, &entry.meta.name, observed)
                    .await
                    .map_err(|e| ApiError::internal("产物状态更新", &e))?;
                entry.meta.state = observed;
            }
        }
        let state_dto = dto_state(&state, &entry.meta);
        items.push(ArtifactDto {
            name: entry.meta.name,
            size: entry.meta.size,
            sha256: entry.meta.sha256,
            created_at: entry.created_at,
            backend: entry.meta.backend.into(),
            job_id: entry.meta.job_id,
            attempt: entry.meta.attempt,
            state: state_dto,
        });
    }
    Ok(Json(BuildArtifactsResponse { items }))
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
        (status = 200, description = "本地产物字节流（Content-Length = 大小，X-Sisyphus-Sha256 = 校验和）", content_type = "application/octet-stream"),
        (status = 302, description = "S3 产物：Location 为短期 GET URL"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需 viewer 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，构建号或产物不存在", body = ErrorBody),
        (status = 409, description = "产物元数据存在但正文缺失或后端尚不可用", body = ErrorBody),
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
    artifact_response(&state, meta).await
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

/// 任务上传声明：优先 jobs.spec_json，缺省回落到构建快照。
async fn ensure_declared_upload(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    name: &str,
) -> Result<(), ApiError> {
    if declared_uploads(job).contains(&name.to_string()) {
        return Ok(());
    }
    let build = BuildRepo::new(state.pool.clone())
        .get(job.build_id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("构建 {} 不存在", job.build_id)))?;
    let snapshot: BuildSnapshot = serde_json::from_str(&build.snapshot)
        .map_err(|e| ApiError::internal("解析构建快照", &e))?;
    let declared = snapshot
        .pipeline
        .stages
        .get(job.stage_index as usize)
        .and_then(|s| s.jobs.iter().find(|j| j.name == job.name))
        .map(|j| {
            j.artifact_uploads
                .iter()
                .map(|u| u.name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if declared.iter().any(|n| n == name) {
        return Ok(());
    }
    Err(ApiError::validation(
        "产物名不在本任务上传声明内",
        vec![ValidationIssue {
            path: "name".into(),
            message: format!("任务 {} 未声明上传 {name}", job.name),
        }],
    ))
}

fn artifact_object_keys(
    prefix: &str,
    job: &crate::store::jobs::JobRow,
    name: &str,
) -> (String, String) {
    let blob = artifact_blob_name(job.build_id, job.id, job.attempt, name);
    (
        object_key(
            prefix,
            ObjectClass::Artifacts,
            ObjectPhase::Temporary,
            &blob,
        ),
        object_key(prefix, ObjectClass::Artifacts, ObjectPhase::Final, &blob),
    )
}

fn declared_uploads(job: &crate::store::jobs::JobRow) -> Vec<String> {
    let Some(spec) = job.spec_json.as_deref() else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(spec) else {
        return Vec::new();
    };
    v.get("artifact_uploads")
        .and_then(|u| u.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("name")
                        .and_then(|n| n.as_str())
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn dto_state(state: &AppState, meta: &ArtifactMeta) -> ArtifactStateDto {
    if meta.backend == ArtifactBackend::S3 && state.s3.is_none() {
        ArtifactStateDto::Unavailable
    } else {
        meta.state.into()
    }
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
async fn artifact_response(state: &AppState, meta: ArtifactMeta) -> Result<Response, ApiError> {
    if meta.state == ArtifactState::Missing {
        return Err(ApiError::conflict(format!("产物 {} 的正文缺失", meta.name)));
    }
    if meta.backend == ArtifactBackend::S3 {
        let Some(s3) = state.s3.as_ref() else {
            return Err(ApiError::conflict(format!(
                "产物 {} 的存储后端未配置",
                meta.name
            )));
        };
        let url = s3
            .presign_get(&meta.path, PRESIGN_SECS)
            .map_err(|e| ApiError::internal("签发下载 URL", &e))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::LOCATION,
            url.parse().expect("预签名 URL 为合法头值"),
        );
        headers.insert(
            header::HeaderName::from_static("x-sisyphus-sha256"),
            meta.sha256.parse().expect("sha256 hex 为合法头值"),
        );
        return Ok((StatusCode::FOUND, headers).into_response());
    }
    if meta.backend != ArtifactBackend::Local {
        return Err(ApiError::conflict(format!(
            "产物 {} 位于 {} 后端，当前下载 adapter 尚不可用",
            meta.name,
            meta.backend.as_str()
        )));
    }
    let stream = match state.artifacts.open(meta.build_id, &meta.name).await {
        Ok(stream) => stream,
        Err(crate::store::StoreError::NotFound(_)) => {
            state
                .artifact_meta
                .set_state(meta.build_id, &meta.name, ArtifactState::Missing)
                .await
                .map_err(|e| ApiError::internal("产物状态更新", &e))?;
            return Err(ApiError::conflict(format!("产物 {} 的正文缺失", meta.name)));
        }
        Err(e) => return Err(ApiError::internal("产物读取", &e)),
    };
    let body = Body::from_stream(stream.map(|r| r.map(axum::body::Bytes::from)));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        meta.size.to_string().parse().expect("长度为合法头值"),
    );
    headers.insert(
        header::HeaderName::from_static("x-sisyphus-sha256"),
        meta.sha256.parse().expect("sha256 hex 为合法头值"),
    );
    // 文件名仅 ASCII 安全字符子集（已过名校验）；attachment 触发浏览器下载。
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", meta.name)
            .parse()
            .expect("产物名为合法头值（无控制字符/引号外字符）"),
    );
    Ok((StatusCode::OK, headers, body).into_response())
}

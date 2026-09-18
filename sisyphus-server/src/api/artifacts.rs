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
use axum::extract::{Extension, Path, Query, Request, State};
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
use super::policy::{RequireAdmin, RequireViewer};
use crate::auth::{TokenFamily, token_family, token_hash};
use crate::storage::{ObjectClass, ObjectPhase, artifact_blob_name, object_key};
use crate::store::artifacts::{ArtifactSetEntry, ArtifactSetRow, MultipartUploadRow};
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
    /// `single` 或 `multipart`。
    pub mode: ArtifactUploadMode,
    /// 单 PUT URL；multipart 时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// multipart upload id；单 PUT 时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_id: Option<String>,
    /// multipart 分片大小；单 PUT 时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_size: Option<u64>,
    /// multipart 分片写 URL；单 PUT时为空数组。
    pub parts: Vec<ArtifactUploadPart>,
    /// 有效秒数。
    pub expires_in: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
/// Agent 应采用的直传模式。
pub enum ArtifactUploadMode {
    /// 单个预签名 PUT。
    Single,
    /// S3 multipart 分片上传。
    Multipart,
}

#[derive(Debug, Serialize, ToSchema)]
/// 一个 multipart 分片的写许可。
pub struct ArtifactUploadPart {
    /// 从 1 开始的分片号。
    pub part_number: u32,
    /// 该分片专用的短期预签名 PUT URL。
    pub url: String,
}

#[derive(Debug, Deserialize, ToSchema)]
/// Agent 申请上传许可时报告的文件信息。
pub struct ArtifactUploadGrantRequest {
    /// Agent 在传输前报告的文件大小（空文件为 0）。
    pub size: u64,
}

/// 任务产物在传输前提交的完整文件大小清单。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactPreflightRequest {
    /// 本次任务将上传的全部单文件声明。
    pub files: Vec<ArtifactPreflightFile>,
}

/// 一条待传输的单文件产物。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactPreflightFile {
    /// 任务上传声明中的名称。
    pub name: String,
    /// 文件大小（字节）。
    pub size: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
/// Agent 已上传分片的完成凭据。
pub struct ArtifactCompletedPart {
    /// 从 1 开始的分片号。
    pub part_number: u32,
    /// S3 UploadPart 响应的 ETag。
    pub etag: String,
}

/// Agent 完成上传：声明的大小与 SHA-256。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactCompleteRequest {
    /// 字节数。
    pub size: u64,
    /// SHA-256 校验和（十六进制小写）。
    pub sha256: String,
    /// multipart grant 返回的 upload id。
    #[serde(default)]
    pub upload_id: Option<String>,
    /// Agent 上传成功的有序分片 ETag。
    #[serde(default)]
    pub parts: Vec<ArtifactCompletedPart>,
}

/// 预签名有效期（秒）：上传 PUT 与用户 GET 同为 5 分钟。
const PRESIGN_SECS: i64 = 5 * 60;
/// multipart 会话需容纳数 GiB 的实际传输时间；写 URL 仍只签发 5 分钟，
/// Agent 重试申请时可在此窗口内复用会话并取得新的分片 URL。
const MULTIPART_SESSION_SECS: i64 = 24 * 60 * 60;

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

/// Agent 提交的一份目录产物清单。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactSetRequest {
    /// 上传声明中的产物名。
    pub name: String,
    /// 完整清单（空目录允许空清单）。
    pub entries: Vec<ArtifactSetInput>,
}

/// 一个普通文件或目录的声明信息。
#[derive(Debug, Deserialize, ToSchema)]
pub struct ArtifactSetInput {
    /// 目录相对路径。
    pub path: String,
    /// file 或 directory。
    pub kind: crate::store::artifacts::ArtifactEntryKind,
    /// 文件大小；目录为零。
    pub size: u64,
    /// 文件 SHA-256；目录为空。
    pub sha256: String,
    /// Unix 可执行位。
    pub executable: bool,
}

/// 目录清单条目及当前可用状态。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactSetEntryDto {
    /// 清单信息。
    #[serde(flatten)]
    pub entry: ArtifactSetEntry,
    /// 正文是否可读；目录没有正文字节，始终 ready。
    pub state: ArtifactStateDto,
}

/// 集合与完整清单。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactSetResponse {
    /// 发布元数据。
    pub set: ArtifactSetRow,
    /// 按路径排序的完整清单。
    pub entries: Vec<ArtifactSetEntryDto>,
    /// 全部文件的聚合可用状态。
    pub availability: ArtifactStateDto,
}

/// 构建内已发布的目录产物列表。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactSetsResponse {
    /// 包含历史 attempt 的集合。
    pub items: Vec<ArtifactSetResponse>,
}

fn valid_set_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && path.split('/').all(|segment| {
            let base = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.ends_with(['.', ' '])
                && !segment
                    .chars()
                    .any(|c| c.is_control() || ":*?\"<>|".contains(c))
                && !matches!(
                    base.as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
}

fn validate_set_entries(
    state: &AppState,
    entries: &[ArtifactSetInput],
) -> Result<Vec<ArtifactSetEntry>, ApiError> {
    let mut paths = std::collections::HashSet::new();
    let mut total = 0_u64;
    let mut files = 0;
    for entry in entries {
        if !valid_set_path(&entry.path) || !paths.insert(entry.path.to_lowercase()) {
            return Err(ApiError::validation(
                "产物集路径非法或跨平台冲突",
                vec![ValidationIssue {
                    path: "entries.path".into(),
                    message: entry.path.clone(),
                }],
            ));
        }
        if entry.kind == crate::store::artifacts::ArtifactEntryKind::File {
            files += 1;
            total = total.saturating_add(entry.size);
            if entry.size > state.artifact_transfer_limits.single_file_limit
                || entry.size > i64::MAX as u64
                || entry.sha256.len() != 64
                || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(ApiError::validation(
                    "产物集文件大小或摘要非法",
                    vec![ValidationIssue {
                        path: "entries".into(),
                        message: entry.path.clone(),
                    }],
                ));
            }
        } else if entry.kind != crate::store::artifacts::ArtifactEntryKind::Directory
            || entry.size != 0
            || !entry.sha256.is_empty()
            || entry.executable
        {
            return Err(ApiError::validation(
                "产物集目录条目非法",
                vec![ValidationIssue {
                    path: "entries".into(),
                    message: entry.path.clone(),
                }],
            ));
        }
    }
    if files > 10_000 || total > state.artifact_transfer_limits.task_limit {
        return Err(ApiError::validation(
            "产物集超出文件数或任务大小限额",
            vec![ValidationIssue {
                path: "entries".into(),
                message: format!("{files} files, {total} bytes"),
            }],
        ));
    }
    for entry in entries {
        let mut parent = entry.path.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('/') {
            if entries.iter().any(|other| {
                other.kind == crate::store::artifacts::ArtifactEntryKind::File
                    && other.path.to_lowercase() == prefix.to_lowercase()
            }) {
                return Err(ApiError::validation(
                    "文件不能作为目录父级",
                    vec![ValidationIssue {
                        path: "entries.path".into(),
                        message: entry.path.clone(),
                    }],
                ));
            }
            parent = prefix;
        }
    }
    let mut result = entries
        .iter()
        .map(|entry| ArtifactSetEntry {
            path: entry.path.clone(),
            kind: entry.kind,
            size: entry.size as i64,
            sha256: entry.sha256.to_ascii_lowercase(),
            executable: entry.executable,
            artifact_name: None,
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

/// 为获派任务创建或复用不可变的目录产物清单。
#[utoipa::path(post, path = "/api/v1/agent/artifacts/{job_id}/sets", tag = "artifacts",
    params(("job_id" = i64, Path, description = "获派任务 ID")), request_body = ArtifactSetRequest,
    responses((status = 200, description = "清单与私有文件名", body = ArtifactSetResponse),
        (status = 404, description = "任务不存在", body = ErrorBody),
        (status = 422, description = "声明或清单非法", body = ErrorBody)))]
pub async fn agent_create_set(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path(job_id): Path<i64>,
    body: axum::body::Bytes,
) -> Result<Json<ArtifactSetResponse>, ApiError> {
    let job = load_own_job(&state, &agent, job_id).await?;
    let req: ArtifactSetRequest = parse_body(&body)?;
    validate_name(&req.name)?;
    if req.name.starts_with(".set-") {
        return Err(ApiError::conflict("内部文件名不能声明为产物集"));
    }
    ensure_declared_upload(&state, &job, &req.name).await?;
    if state
        .artifact_meta
        .find_including_pending_for_job(job.build_id, job.id, job.attempt, &req.name)
        .await?
        .is_some()
    {
        return Err(ApiError::conflict("同名产物已经作为普通文件上传"));
    }
    let entries = validate_set_entries(&state, &req.entries)?;
    let (set, entries) = state
        .artifact_meta
        .create_set(job.build_id, job.id, job.attempt, &req.name, &entries)
        .await
        .map_err(|e| {
            ApiError::validation(
                "产物集清单冲突",
                vec![ValidationIssue {
                    path: "entries".into(),
                    message: e.to_string(),
                }],
            )
        })?;
    Ok(Json(set_response(&state, set, entries).await?))
}

/// 核验清单所有文件后原子发布整个集合。
#[utoipa::path(post, path = "/api/v1/agent/artifacts/{job_id}/sets/{set_id}/publish", tag = "artifacts",
    params(("job_id" = i64, Path, description = "获派任务 ID"), ("set_id" = i64, Path, description = "集合 ID")),
    responses((status = 201, description = "完整发布"), (status = 200, description = "幂等发布"),
        (status = 404, description = "集合不存在", body = ErrorBody),
        (status = 409, description = "清单尚未完整", body = ErrorBody)))]
pub async fn agent_publish_set(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, set_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ApiError> {
    let job = load_own_job(&state, &agent, job_id).await?;
    let set = state
        .artifact_meta
        .set(set_id)
        .await?
        .filter(|s| s.job_id == job.id && s.attempt == job.attempt)
        .ok_or_else(|| ApiError::resource_not_found("产物集不存在"))?;
    if set.state == crate::store::artifacts::ArtifactSetState::Ready {
        return Ok(StatusCode::OK);
    }
    state
        .artifact_meta
        .publish_set(set_id)
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    Ok(StatusCode::CREATED)
}

/// 构建详情的完整目录产物及文件可用状态。
#[utoipa::path(get, path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/artifact-sets", tag = "artifacts",
    params(("name" = String, Path, description = "项目"), ("pipeline" = String, Path, description = "流水线"),
        ("number" = i64, Path, description = "构建号")),
    responses((status = 200, description = "已发布目录集合", body = ArtifactSetsResponse),
        (status = 403, description = "权限不足", body = ErrorBody), (status = 404, description = "构建不存在", body = ErrorBody)))]
pub async fn list_sets(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number)): Path<(String, String, i64)>,
) -> Result<Json<ArtifactSetsResponse>, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let mut items = Vec::new();
    for set in state.artifact_meta.list_sets(build.id).await? {
        let entries = state.artifact_meta.set_entries(set.id).await?;
        items.push(set_response(&state, set, entries).await?);
    }
    Ok(Json(ArtifactSetsResponse { items }))
}

/// 删除整个 ready 产物集。删除先持久化任务并立即从列表/下载面隐藏，正文
/// 由后台幂等清理；排队或运行中的构建拒绝清理。
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/artifact-sets/{set_id}",
    tag = "artifacts",
    responses(
        (status = 202, description = "已进入异步删除", body = crate::store::deletions::DeletionJob),
        (status = 403, description = "需项目 admin 档", body = ErrorBody),
        (status = 404, description = "构建或产物集不存在", body = ErrorBody),
        (status = 409, description = "排队/运行中的构建不可清理", body = ErrorBody),
    )
)]
pub async fn delete_set(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, pipeline, number, set_id)): Path<(String, String, i64, i64)>,
) -> Result<(StatusCode, Json<crate::store::deletions::DeletionJob>), ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    if !build.status.is_terminal() {
        return Err(ApiError::conflict(format!(
            "构建 #{number} 运行中/排队中，不可删除产物集"
        )));
    }
    let job = state
        .deletions
        .enqueue_set(access.project.id, build.id, set_id, &access.operator)
        .await
        .map_err(|error| match error {
            crate::store::StoreError::NotFound(_) => ApiError::resource_not_found("产物集不存在"),
            other => ApiError::internal("产物集删除入队", &other),
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// 清单文件的精确相对路径查询。
#[derive(Debug, Deserialize)]
pub struct SetFileQuery {
    /// 不进行模糊或前缀匹配的相对路径。
    pub path: String,
}

/// 项目授权后下载 ready 集合内的单个文件。
#[utoipa::path(get, path = "/api/v1/projects/{name}/pipelines/{pipeline}/builds/{number}/artifact-sets/{set_id}/file", tag = "artifacts",
    params(("name" = String, Path, description = "项目"), ("pipeline" = String, Path, description = "流水线"),
        ("number" = i64, Path, description = "构建号"), ("set_id" = i64, Path, description = "集合 ID"),
        ("path" = String, Query, description = "清单相对路径")),
    responses((status = 200, description = "本地文件流", content_type = "application/octet-stream"),
        (status = 302, description = "S3 短期 GET URL"), (status = 403, description = "权限不足", body = ErrorBody),
        (status = 404, description = "文件不存在", body = ErrorBody), (status = 409, description = "正文不可用", body = ErrorBody)))]
pub async fn download_set_file(
    State(state): State<AppState>,
    RequireViewer(access): RequireViewer,
    Path((_project, pipeline, number, set_id)): Path<(String, String, i64, i64)>,
    Query(query): Query<SetFileQuery>,
) -> Result<Response, ApiError> {
    let build = load_build(&state, &access.project.id, &pipeline, number).await?;
    let set = state
        .artifact_meta
        .set(set_id)
        .await?
        .filter(|s| {
            s.build_id == build.id && s.state == crate::store::artifacts::ArtifactSetState::Ready
        })
        .ok_or_else(|| ApiError::resource_not_found("产物集不存在"))?;
    let entry = state
        .artifact_meta
        .set_entries(set.id)
        .await?
        .into_iter()
        .find(|e| {
            e.path == query.path && e.kind == crate::store::artifacts::ArtifactEntryKind::File
        })
        .ok_or_else(|| ApiError::resource_not_found("产物文件不存在"))?;
    let name = entry
        .artifact_name
        .ok_or_else(|| ApiError::resource_not_found("产物文件不存在"))?;
    let meta = state
        .artifact_meta
        .find(build.id, &name)
        .await?
        .ok_or_else(|| ApiError::conflict("产物正文尚未就绪"))?;
    artifact_response(&state, meta).await
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
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
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

/// 在开始传输任务的第一件产物前，校验完整清单的单文件与合计限额。
#[utoipa::path(
    post,
    path = "/api/v1/agent/artifacts/{job_id}/preflight",
    tag = "artifacts",
    request_body = ArtifactPreflightRequest,
    params(("job_id" = i64, Path, description = "上传任务自身行 id")),
    responses(
        (status = 204, description = "整任务清单已通过限额校验"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 404, description = "任务行不存在", body = ErrorBody),
        (status = 422, description = "未声明产物或传输限额超出", body = ErrorBody),
    )
)]
pub async fn agent_preflight(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path(job_id): Path<i64>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    let job = load_own_job(&state, &agent, job_id).await?;

    if state.s3.is_none() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let request: ArtifactPreflightRequest = parse_body(&body)?;
    let mut seen = std::collections::HashSet::new();
    let mut total = 0_u64;
    for file in &request.files {
        validate_name(&file.name)?;
        ensure_declared_upload(&state, &job, &file.name).await?;
        ensure_no_set_collision(&state, &job, &file.name).await?;
        if !seen.insert(&file.name) {
            return Err(ApiError::validation(
                "上传清单含重复产物名",
                vec![ValidationIssue {
                    path: "files".into(),
                    message: format!("重复的产物名：{}", file.name),
                }],
            ));
        }
        if file.size > state.artifact_transfer_limits.single_file_limit {
            return Err(ApiError::validation(
                "产物超过单文件限额",
                vec![ValidationIssue {
                    path: "files".into(),
                    message: format!(
                        "{} 为 {} 字节，限额 {} 字节",
                        file.name, file.size, state.artifact_transfer_limits.single_file_limit
                    ),
                }],
            ));
        }
        total = total.saturating_add(file.size);
    }
    if total > state.artifact_transfer_limits.task_limit {
        return Err(ApiError::validation(
            "产物超过单任务限额",
            vec![ValidationIssue {
                path: "files".into(),
                message: format!(
                    "任务合计 {total} 字节，限额 {} 字节",
                    state.artifact_transfer_limits.task_limit
                ),
            }],
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

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

    let expected = if name.starts_with(".set-") {
        ensure_declared_upload(&state, &job, &name).await?;
        if state
            .artifact_meta
            .find_for_job(job.build_id, job.id, job.attempt, &name)
            .await?
            .is_some()
        {
            return Err(ApiError::conflict("已校验的集合文件不能覆盖"));
        }
        state
            .artifact_meta
            .pending_set_file(job.build_id, job.id, job.attempt, &name)
            .await?
    } else {
        ensure_no_set_collision(&state, &job, &name).await?;
        None
    };

    // 请求体流 → 字节流缝（axum DataStream 的错误归一为 io::Error；Bytes
    // → Vec 与缝的元素型对齐）。
    let stream = body
        .into_data_stream()
        .map(|r| r.map(|b| b.to_vec()).map_err(std::io::Error::other))
        .boxed();
    let stored = if let Some(expected) = &expected {
        enforce_transfer_limits(&state, &job, &name, expected.size as u64).await?;
        state
            .artifacts
            .store_verified(
                job.build_id,
                &name,
                stream,
                expected.size as u64,
                &expected.sha256,
            )
            .await
    } else {
        // 保持首个/同任务上传的历史磁盘布局；仅在构建内已有其它任务同名
        // 产物时切换到任务隔离键，兼容旧运维脚本与已有本地产物。
        let collision = state
            .artifact_meta
            .list_by_build(job.build_id)
            .await?
            .into_iter()
            .any(|meta| {
                meta.name == name
                    && (meta.job_id != Some(job.id) || meta.attempt != Some(job.attempt))
            });
        if collision {
            state
                .artifacts
                .store_for_job(job.build_id, job.id, job.attempt, &name, stream)
                .await
        } else {
            state.artifacts.store(job.build_id, &name, stream).await
        }
    };
    let mut meta = stored.map_err(|e| ApiError::conflict(e.to_string()))?;
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
    request_body = ArtifactUploadGrantRequest,
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
    body: axum::body::Bytes,
) -> Result<Json<ArtifactUploadUrlResponse>, ApiError> {
    validate_name(&name)?;
    let job = load_own_job(&state, &agent, job_id).await?;
    let s3 = state
        .s3
        .as_ref()
        .ok_or_else(|| ApiError::conflict("未配置 S3，无法签发直传 URL"))?;
    cleanup_expired_multipart_uploads(&state).await?;
    ensure_declared_upload(&state, &job, &name).await?;
    ensure_no_set_collision(&state, &job, &name).await?;
    let request: ArtifactUploadGrantRequest = parse_body(&body)?;
    ensure_set_file_content(&state, &job, &name, request.size, None).await?;
    enforce_transfer_limits(&state, &job, &name, request.size).await?;

    let (tmp_key, final_key) = artifact_object_keys(s3.prefix(), &job, &name);
    let existing = state
        .artifact_meta
        .find_including_pending_for_job(job.build_id, job.id, job.attempt, &name)
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
            size: request.size,
            sha256: String::new(),
        })
        .await
        .map_err(|e| ApiError::internal("产物 pending 落库", &e))?;
    let limits = state.artifact_transfer_limits;
    if request.size >= limits.multipart_threshold && request.size > 0 {
        let part_size =
            crate::storage::s3::multipart_part_size(request.size, limits.multipart_part_size);
        let part_count = request.size.div_ceil(part_size);
        let now = crate::store::now_ms();
        let upload_id = match state
            .artifact_meta
            .find_multipart(job.build_id, job.id, job.attempt, &name)
            .await?
        {
            Some(existing)
                if existing.job_id == job.id
                    && existing.attempt == job.attempt
                    && existing.object_key == tmp_key
                    && existing.size == request.size
                    && existing.part_size == part_size
                    && existing.expires_at > now =>
            {
                existing.upload_id
            }
            existing => {
                if let Some(existing) = existing {
                    if !existing.completed {
                        s3.abort_multipart_upload(&existing.object_key, &existing.upload_id)
                            .await
                            .map_err(|e| ApiError::internal("中止旧 multipart 上传", &e))?;
                    }
                    s3.delete_object(&existing.object_key)
                        .await
                        .map_err(|e| ApiError::internal("清理旧临时对象", &e))?;
                    state
                        .artifact_meta
                        .delete_multipart(job.build_id, job.id, job.attempt, &name)
                        .await?;
                }
                let upload_id = s3
                    .create_multipart_upload(&tmp_key)
                    .await
                    .map_err(|e| ApiError::internal("创建 multipart 上传", &e))?;
                state
                    .artifact_meta
                    .record_multipart(&MultipartUploadRow {
                        build_id: job.build_id,
                        name: name.clone(),
                        job_id: job.id,
                        attempt: job.attempt,
                        object_key: tmp_key.clone(),
                        upload_id: upload_id.clone(),
                        size: request.size,
                        part_size,
                        completed: false,
                        expires_at: now + MULTIPART_SESSION_SECS * 1000,
                    })
                    .await?;
                upload_id
            }
        };
        let mut parts = Vec::with_capacity(part_count as usize);
        for part_number in 1..=part_count as u32 {
            let url = match s3.presign_upload_part(&tmp_key, &upload_id, part_number, PRESIGN_SECS)
            {
                Ok(url) => url,
                Err(error) => {
                    let _ = s3.abort_multipart_upload(&tmp_key, &upload_id).await;
                    return Err(ApiError::internal("签发 multipart 分片 URL", &error));
                }
            };
            parts.push(ArtifactUploadPart { part_number, url });
        }
        return Ok(Json(ArtifactUploadUrlResponse {
            mode: ArtifactUploadMode::Multipart,
            url: None,
            upload_id: Some(upload_id),
            part_size: Some(part_size),
            parts,
            expires_in: PRESIGN_SECS,
        }));
    }
    if let Some(existing) = state
        .artifact_meta
        .find_multipart(job.build_id, job.id, job.attempt, &name)
        .await?
    {
        if !existing.completed {
            s3.abort_multipart_upload(&existing.object_key, &existing.upload_id)
                .await
                .map_err(|e| ApiError::internal("中止旧 multipart 上传", &e))?;
        }
        s3.delete_object(&existing.object_key)
            .await
            .map_err(|e| ApiError::internal("清理旧临时对象", &e))?;
        state
            .artifact_meta
            .delete_multipart(job.build_id, job.id, job.attempt, &name)
            .await?;
    }
    let url = s3
        .presign_put(&tmp_key, PRESIGN_SECS)
        .map_err(|e| ApiError::internal("签发上传 URL", &e))?;
    Ok(Json(ArtifactUploadUrlResponse {
        mode: ArtifactUploadMode::Single,
        url: Some(url),
        upload_id: None,
        part_size: None,
        parts: Vec::new(),
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
    cleanup_expired_multipart_uploads(&state).await?;
    let req: ArtifactCompleteRequest = parse_body(&body)?;
    let sha = req.sha256.to_ascii_lowercase();

    if let Some(meta) = state
        .artifact_meta
        .find_including_pending_for_job(job.build_id, job.id, job.attempt, &name)
        .await?
        && meta.state == ArtifactState::Ready
        && meta.backend == ArtifactBackend::S3
        && meta.job_id == Some(job.id)
        && meta.attempt == Some(job.attempt)
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
    ensure_declared_upload(&state, &job, &name).await?;
    ensure_set_file_content(&state, &job, &name, req.size, Some(&sha)).await?;

    enforce_transfer_limits(&state, &job, &name, req.size).await?;
    let pending = state
        .artifact_meta
        .find_including_pending_for_job(job.build_id, job.id, job.attempt, &name)
        .await?
        .filter(|meta| {
            meta.state == ArtifactState::Pending
                && meta.backend == ArtifactBackend::S3
                && meta.job_id == Some(job.id)
                && meta.attempt == Some(job.attempt)
                && meta.size == req.size
        })
        .ok_or_else(|| {
            ApiError::validation(
                "上传许可与完成请求不一致",
                vec![ValidationIssue {
                    path: "size".into(),
                    message: "请按已签发许可的文件大小重新申请上传".into(),
                }],
            )
        })?;

    let (tmp_key, final_key) = artifact_object_keys(s3.prefix(), &job, &name);
    if pending.path != final_key {
        return Err(ApiError::conflict("上传许可的最终对象不一致"));
    }

    if let Some(upload_id) = req.upload_id.as_deref() {
        if req.parts.is_empty() {
            return Err(ApiError::validation(
                "multipart 完成清单为空",
                vec![ValidationIssue {
                    path: "parts".into(),
                    message: "至少需要一个分片 ETag".into(),
                }],
            ));
        }
        let session = state
            .artifact_meta
            .find_multipart(job.build_id, job.id, job.attempt, &name)
            .await?
            .filter(|session| {
                session.upload_id == upload_id
                    && session.job_id == job.id
                    && session.attempt == job.attempt
                    && session.object_key == tmp_key
                    && session.size == req.size
            })
            .ok_or_else(|| {
                ApiError::validation(
                    "multipart 上传会话不存在或已过期",
                    vec![ValidationIssue {
                        path: "upload_id".into(),
                        message: "请重新申请上传许可".into(),
                    }],
                )
            })?;
        let mut parts = req
            .parts
            .iter()
            .map(|part| (part.part_number, part.etag.clone()))
            .collect::<Vec<_>>();
        parts.sort_by_key(|(number, _)| *number);
        if !session.completed {
            if let Err(error) = s3
                .complete_multipart_upload(&tmp_key, upload_id, &parts)
                .await
            {
                // S3 可能完成了对象但响应丢失；仅当完整哈希符合本次请求
                // 时才把它视作已完成，否则保留会话供再次重试。
                if s3.hash_object(&tmp_key).await.ok() != Some((req.size, sha.clone())) {
                    return Err(ApiError::internal("完成 multipart 上传", &error));
                }
            }
            state
                .artifact_meta
                .mark_multipart_completed(session.build_id, job.id, job.attempt, &session.name)
                .await?;
        }
    } else if !req.parts.is_empty() {
        return Err(ApiError::validation(
            "multipart upload id 缺失",
            vec![ValidationIssue {
                path: "upload_id".into(),
                message: "提交分片清单时必须提供 upload_id".into(),
            }],
        ));
    }

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
    s3.copy_object_adaptive(
        &tmp_key,
        &final_key,
        size,
        state.artifact_transfer_limits.copy_object_limit,
        state.artifact_transfer_limits.copy_part_size,
    )
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
    if req.upload_id.is_some() {
        state
            .artifact_meta
            .delete_multipart(job.build_id, job.id, job.attempt, &name)
            .await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(ArtifactUploadedResponse {
            name: meta.name,
            size: meta.size,
            sha256: meta.sha256,
        }),
    ))
}

/// abort 并删除全部已过期 multipart 会话。失败行保留，供后台/下次请求重试。
pub(crate) async fn cleanup_expired_multipart_uploads(state: &AppState) -> Result<(), ApiError> {
    let Some(s3) = state.s3.as_ref() else {
        return Ok(());
    };
    let expired = state
        .artifact_meta
        .list_expired_multipart(crate::store::now_ms())
        .await?;
    for upload in expired {
        let aborted = upload.completed
            || s3
                .abort_multipart_upload(&upload.object_key, &upload.upload_id)
                .await
                .is_ok();
        if aborted && s3.delete_object(&upload.object_key).await.is_ok() {
            state
                .artifact_meta
                .cleanup_expired_multipart(&upload)
                .await?;
        }
    }
    Ok(())
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

    if let Some(set) = state
        .artifact_meta
        .list_sets(build_id)
        .await?
        .into_iter()
        .filter(|s| {
            s.name == name
                && jobs
                    .iter()
                    .any(|j| j.id == s.job_id && j.name == source_job)
        })
        .max_by_key(|s| s.attempt)
    {
        let entries = state.artifact_meta.set_entries(set.id).await?;
        ensure_declared_download(&state, &job, &source_job, &name).await?;
        return Ok(Json(set_response(&state, set, entries).await?).into_response());
    }

    let mut source_attempts: Vec<&crate::store::jobs::JobRow> = jobs
        .iter()
        .filter(|candidate| candidate.name == source_job)
        .collect();
    if source_attempts.is_empty() {
        return Err(ApiError::resource_not_found(format!(
            "来源任务 {source_job} 不存在"
        )));
    }
    source_attempts.sort_by_key(|candidate| std::cmp::Reverse(candidate.attempt));
    let mut meta = None;
    for source in source_attempts {
        if let Some(found) = state
            .artifact_meta
            .find_for_job(build_id, source.id, source.attempt, &name)
            .await?
            && found.state != ArtifactState::Pending
        {
            meta = Some(found);
            break;
        }
    }
    // 迁移前的本地历史产物没有任务归属，只能按构建/名称兼容读取；新上传
    // 一旦带有归属就不会走此回退，避免跨任务同名串读。
    if meta.is_none()
        && let Some(legacy) = state.artifact_meta.find(build_id, &name).await?
        && legacy.job_id.is_none()
    {
        meta = Some(legacy);
    }
    let meta = meta.ok_or_else(|| {
        // 「依赖产物尚不存在」的清晰报错（票 #74 AC）：Agent 侧据此
        // 任务失败，不静默空等。
        ApiError::resource_not_found(format!(
            "依赖产物尚不存在：任务 {source_job} 的产物 {name} 未上传"
        ))
    })?;
    if name.starts_with(".set-") {
        return Err(ApiError::resource_not_found(
            "目录产物只能在整组 ready 后读取",
        ));
    }
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
        if entry.meta.name.starts_with(".set-") {
            continue;
        }
        if entry.meta.backend == ArtifactBackend::Local {
            let observed = state
                .artifacts
                .inspect_state(&entry.meta)
                .await
                .map_err(|e| ApiError::internal("产物状态检查", &e))?;
            if observed != entry.meta.state {
                if let (Some(job_id), Some(attempt)) = (entry.meta.job_id, entry.meta.attempt) {
                    state
                        .artifact_meta
                        .set_state_for_job(
                            entry.meta.build_id,
                            job_id,
                            attempt,
                            &entry.meta.name,
                            observed,
                        )
                        .await
                } else {
                    state
                        .artifact_meta
                        .set_state(entry.meta.build_id, &entry.meta.name, observed)
                        .await
                }
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
    if artifact.starts_with(".set-") {
        return Err(ApiError::resource_not_found("产物不存在"));
    }
    let matches: Vec<_> = state
        .artifact_meta
        .list_by_build(build.id)
        .await?
        .into_iter()
        .filter(|meta| meta.name == artifact)
        .collect();
    let meta = match matches.as_slice() {
        [] => {
            return Err(ApiError::resource_not_found(format!(
                "产物 {artifact} 不存在"
            )));
        }
        [meta] => meta.clone(),
        _ => {
            return Err(ApiError::conflict(format!(
                "产物 {artifact} 存在多个任务版本，请使用任务范围接口"
            )));
        }
    };
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
    let job = JobRepo::new(state.pool.clone())
        .get(job_id)
        .await?
        .filter(|j| j.agent_id == Some(agent.agent_id))
        .ok_or_else(|| ApiError::resource_not_found(format!("任务 {job_id} 不存在")))?;
    let project_active: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM builds b JOIN projects p ON p.id = b.project_id
            WHERE b.id = ? AND p.lifecycle = 'active'
        )",
    )
    .bind(job.build_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|error| ApiError::internal("项目生命周期查询", &error))?;
    if !project_active {
        return Err(ApiError::resource_not_found(format!(
            "任务 {job_id} 不存在"
        )));
    }
    Ok(job)
}

/// 任务上传声明：优先 jobs.spec_json，缺省回落到构建快照。
async fn ensure_declared_upload(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    name: &str,
) -> Result<(), ApiError> {
    if name.starts_with(".set-") {
        if state
            .artifact_meta
            .pending_set_file(job.build_id, job.id, job.attempt, name)
            .await?
            .is_some()
        {
            return Ok(());
        }
        return Err(ApiError::resource_not_found("产物集文件不存在"));
    }
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

async fn ensure_no_set_collision(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    name: &str,
) -> Result<(), ApiError> {
    if !name.starts_with(".set-")
        && state
            .artifact_meta
            .set_by_name(job.id, job.attempt, name)
            .await?
            .is_some()
    {
        return Err(ApiError::conflict("同名产物已经作为目录清单创建"));
    }
    Ok(())
}

async fn ensure_declared_download(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    source_job: &str,
    name: &str,
) -> Result<(), ApiError> {
    let from_spec = job
        .spec_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| {
            value
                .get("artifact_downloads")
                .and_then(|v| v.as_array())
                .cloned()
        })
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("job").and_then(|v| v.as_str()) == Some(source_job)
                    && item.get("name").and_then(|v| v.as_str()) == Some(name)
            })
        });
    if from_spec {
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
        .and_then(|stage| {
            stage
                .jobs
                .iter()
                .find(|candidate| candidate.name == job.name)
        })
        .is_some_and(|candidate| {
            candidate
                .artifact_downloads
                .iter()
                .any(|download| download.job == source_job && download.name == name)
        });
    if declared {
        return Ok(());
    }
    Err(ApiError::resource_not_found("依赖产物不在本任务下载声明内"))
}

async fn ensure_set_file_content(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    name: &str,
    size: u64,
    sha256: Option<&str>,
) -> Result<(), ApiError> {
    if !name.starts_with(".set-") {
        return Ok(());
    }
    let entry = state
        .artifact_meta
        .pending_set_file(job.build_id, job.id, job.attempt, name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found("产物集文件不存在"))?;
    if size != entry.size as u64 || sha256.is_some_and(|sha| sha != entry.sha256) {
        return Err(ApiError::conflict("文件大小或摘要与不可变清单不一致"));
    }
    Ok(())
}

async fn set_response(
    state: &AppState,
    set: ArtifactSetRow,
    entries: Vec<ArtifactSetEntry>,
) -> Result<ArtifactSetResponse, ApiError> {
    let mut items = Vec::with_capacity(entries.len());
    let mut availability = ArtifactStateDto::Ready;
    for entry in entries {
        let observed = if let Some(name) = entry.artifact_name.as_deref() {
            match state
                .artifact_meta
                .find_for_job(set.build_id, set.job_id, set.attempt, name)
                .await?
            {
                Some(meta) if meta.backend == ArtifactBackend::S3 => match &state.s3 {
                    None => ArtifactStateDto::Unavailable,
                    Some(s3) => match s3.head_object(&meta.path).await {
                        Ok(size) if size == meta.size => ArtifactStateDto::Ready,
                        Ok(_) => ArtifactStateDto::Missing,
                        Err(crate::storage::StorageError::MissingBucket(_)) => {
                            ArtifactStateDto::Missing
                        }
                        Err(crate::storage::StorageError::MissingObject(_)) => {
                            ArtifactStateDto::Missing
                        }
                        Err(_) => ArtifactStateDto::Unavailable,
                    },
                },
                Some(meta) => state.artifacts.inspect_state(&meta).await?.into(),
                None => ArtifactStateDto::Missing,
            }
        } else {
            ArtifactStateDto::Ready
        };
        availability = match (availability, observed) {
            (ArtifactStateDto::Unavailable, _) | (_, ArtifactStateDto::Unavailable) => {
                ArtifactStateDto::Unavailable
            }
            (ArtifactStateDto::Missing, _) | (_, ArtifactStateDto::Missing) => {
                ArtifactStateDto::Missing
            }
            _ => ArtifactStateDto::Ready,
        };
        items.push(ArtifactSetEntryDto {
            entry,
            state: observed,
        });
    }
    Ok(ArtifactSetResponse {
        set,
        entries: items,
        availability,
    })
}

/// 同构建内的获派 Agent 下载完整发布集合的单个清单文件。
#[utoipa::path(get, path = "/api/v1/agent/artifacts/{job_id}/sets/{set_id}/file", tag = "artifacts",
    params(("job_id" = i64, Path, description = "消费任务 ID"), ("set_id" = i64, Path, description = "集合 ID"),
        ("path" = String, Query, description = "清单相对路径")),
    responses((status = 200, description = "文件流", content_type = "application/octet-stream"),
        (status = 302, description = "短期 GET URL"), (status = 404, description = "集合或文件不存在", body = ErrorBody),
        (status = 409, description = "正文不可用", body = ErrorBody)))]
pub async fn agent_set_file(
    State(state): State<AppState>,
    Extension(agent): Extension<AgentAuth>,
    Path((job_id, set_id)): Path<(i64, i64)>,
    Query(query): Query<SetFileQuery>,
) -> Result<Response, ApiError> {
    let job = load_own_job(&state, &agent, job_id).await?;
    let set = state
        .artifact_meta
        .set(set_id)
        .await?
        .filter(|s| {
            s.build_id == job.build_id
                && s.state == crate::store::artifacts::ArtifactSetState::Ready
        })
        .ok_or_else(|| ApiError::resource_not_found("产物集不存在"))?;
    let source = JobRepo::new(state.pool.clone())
        .get(set.job_id)
        .await?
        .ok_or_else(|| ApiError::resource_not_found("来源任务不存在"))?;
    ensure_declared_download(&state, &job, &source.name, &set.name).await?;
    let entry = state
        .artifact_meta
        .set_entries(set.id)
        .await?
        .into_iter()
        .find(|e| {
            e.path == query.path && e.kind == crate::store::artifacts::ArtifactEntryKind::File
        })
        .ok_or_else(|| ApiError::resource_not_found("清单文件不存在"))?;
    let meta = state
        .artifact_meta
        .find_for_job(
            set.build_id,
            set.job_id,
            set.attempt,
            &entry.artifact_name.unwrap_or_default(),
        )
        .await?
        .ok_or_else(|| ApiError::conflict("产物正文缺失"))?;
    artifact_response(&state, meta).await
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

async fn enforce_transfer_limits(
    state: &AppState,
    job: &crate::store::jobs::JobRow,
    name: &str,
    size: u64,
) -> Result<(), ApiError> {
    let limits = state.artifact_transfer_limits;
    if size > limits.single_file_limit {
        return Err(ApiError::validation(
            "产物超过单文件限额",
            vec![ValidationIssue {
                path: "size".into(),
                message: format!("文件 {size} 字节，限额 {} 字节", limits.single_file_limit),
            }],
        ));
    }
    let existing = state
        .artifact_meta
        .other_upload_bytes(job.id, job.attempt, name)
        .await
        .map_err(|e| ApiError::internal("计算任务产物大小", &e))?;
    let total = existing.saturating_add(size);
    if total > limits.task_limit {
        return Err(ApiError::validation(
            "产物超过单任务限额",
            vec![ValidationIssue {
                path: "size".into(),
                message: format!("任务合计 {total} 字节，限额 {} 字节", limits.task_limit),
            }],
        ));
    }
    Ok(())
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
    let stream = match state.artifacts.open_meta(&meta).await {
        Ok(stream) => stream,
        Err(crate::store::StoreError::NotFound(_)) => {
            if let (Some(job_id), Some(attempt)) = (meta.job_id, meta.attempt) {
                state
                    .artifact_meta
                    .set_state_for_job(
                        meta.build_id,
                        job_id,
                        attempt,
                        &meta.name,
                        ArtifactState::Missing,
                    )
                    .await
            } else {
                state
                    .artifact_meta
                    .set_state(meta.build_id, &meta.name, ArtifactState::Missing)
                    .await
            }
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

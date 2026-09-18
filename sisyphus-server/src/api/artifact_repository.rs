//! 一级制品库入口状态（票 #122，ADR-0026）：未配置 S3 时明确不可用。

use axum::Json;
use axum::extract::{Query, State};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde::Serialize;
use utoipa::{IntoParams, ToSchema};

use super::AppState;
use super::auth::AuthContext;
use super::error::ApiError;
use crate::store::artifacts::{ArtifactRepositoryFilter, ArtifactRepositoryItemRow};

/// 一级制品库入口状态。
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactRepositoryStatus {
    /// 是否已配置且启动校验通过的 S3 后端。
    pub available: bool,
    /// 不可用原因（available=true 时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 非机密后端摘要（available=false 时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<S3BackendDto>,
}

/// 普通 API 可见的 S3 后端摘要（无凭据）。
#[derive(Debug, Serialize, ToSchema)]
pub struct S3BackendDto {
    /// Endpoint。
    pub endpoint: String,
    /// Region。
    pub region: String,
    /// Bucket。
    pub bucket: String,
    /// 根前缀。
    pub prefix: String,
    /// path-style。
    pub path_style: bool,
}

/// 一级制品库筛选与分页参数。
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct ArtifactRepositoryQuery {
    /// 项目名。
    pub project: Option<String>,
    /// Pipeline 名。
    pub pipeline: Option<String>,
    /// 构建号。
    pub build: Option<i64>,
    /// 任务名。
    pub job: Option<String>,
    /// attempt 序号。
    pub attempt: Option<i32>,
    /// 产物名或目录路径的包含匹配。
    pub name: Option<String>,
    /// 页码（从 1 开始）。
    pub page: Option<i64>,
    /// 每页条数，默认 50，最大 100。
    pub limit: Option<i64>,
}

/// 聚合条目的来源定位。
#[allow(missing_docs)]
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactRepositorySource {
    pub project: String,
    pub pipeline: String,
    pub build: i64,
    pub job: Option<String>,
    pub attempt: Option<i32>,
}

/// 一级制品库中的一条文件/目录条目。
#[allow(missing_docs)]
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactRepositoryItem {
    /// `file` 或 `set_entry`。
    pub kind: String,
    /// 产物集 ID 或旧式单文件 ID。
    pub id: i64,
    pub name: String,
    pub set_name: Option<String>,
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
    pub backend: String,
    /// 正文可用状态（当前聚合查询只返回 `ready`）。
    pub availability: String,
    pub created_at: i64,
    pub source: ArtifactRepositorySource,
    /// 可下载文件的同源 URL；目录条目为空。
    pub download_url: Option<String>,
}

/// 一级制品库分页响应。
#[allow(missing_docs)]
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactRepositoryResponse {
    pub items: Vec<ArtifactRepositoryItem>,
    pub total: i64,
    pub page: i64,
    pub limit: i64,
    /// 可见项目中仍存在的旧本地产物数量。
    pub legacy_local_count: i64,
}

/// 一级制品库入口状态（任意登录角色）。
#[utoipa::path(
    get,
    path = "/api/v1/artifact-repository",
    tag = "artifacts",
    responses(
        (status = 200, body = ArtifactRepositoryStatus, description = "制品库是否可用（未配置 S3 时 available=false）"),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
    )
)]
pub async fn status(
    State(state): State<AppState>,
    axum::Extension(_auth): axum::Extension<AuthContext>,
) -> Result<Json<ArtifactRepositoryStatus>, ApiError> {
    Ok(Json(match state.s3.as_ref() {
        Some(s3) => {
            let view = s3.public_view();
            ArtifactRepositoryStatus {
                available: true,
                reason: None,
                backend: Some(S3BackendDto {
                    endpoint: view.endpoint,
                    region: view.region,
                    bucket: view.bucket,
                    prefix: view.prefix,
                    path_style: view.path_style,
                }),
            }
        }
        None => ArtifactRepositoryStatus {
            available: false,
            reason: Some("s3_unconfigured".into()),
            backend: None,
        },
    }))
}

/// 按项目聚合浏览 S3 历史构建产物。
#[utoipa::path(
    get,
    path = "/api/v1/artifact-repository/artifacts",
    tag = "artifacts",
    params(ArtifactRepositoryQuery),
    responses(
        (status = 200, body = ArtifactRepositoryResponse, description = "按项目聚合的 S3 产物条目"),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
        (status = 409, description = "未配置 S3", body = super::error::ErrorBody),
        (status = 422, description = "分页参数非法", body = super::error::ErrorBody)
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(query): Query<ArtifactRepositoryQuery>,
) -> Result<Json<ArtifactRepositoryResponse>, ApiError> {
    if state.s3.is_none() {
        return Err(ApiError::conflict("制品库不可用：尚未配置 S3"));
    }
    let page = query.page.unwrap_or(1);
    let limit = query.limit.unwrap_or(50);
    if page < 1 || !(1..=100).contains(&limit) {
        return Err(ApiError::validation(
            "制品库分页参数非法",
            vec![super::error::ValidationIssue {
                path: "page/limit".into(),
                message: "page >= 1 且 limit 在 1..=100 之间".into(),
            }],
        ));
    }
    let projects = state
        .projects
        .list_visible(auth.is_admin, auth.user_id)
        .await?;
    let project_ids: Vec<i64> = projects.iter().map(|project| project.id).collect();
    let filter = ArtifactRepositoryFilter {
        project: query.project.clone(),
        pipeline: query.pipeline.clone(),
        build: query.build,
        job: query.job.clone(),
        attempt: query.attempt,
        name: query.name.clone(),
    };
    let rows = state
        .artifact_meta
        .list_repository_items(&project_ids, &filter)
        .await?;
    let total = rows.len() as i64;
    let offset = ((page - 1) * limit) as usize;
    let items = rows
        .into_iter()
        .skip(offset)
        .take(limit as usize)
        .map(repository_item)
        .collect();
    let legacy_local_count = state
        .artifact_meta
        .count_local_artifacts(&project_ids)
        .await?;
    Ok(Json(ArtifactRepositoryResponse {
        items,
        total,
        page,
        limit,
        legacy_local_count,
    }))
}

fn repository_item(row: ArtifactRepositoryItemRow) -> ArtifactRepositoryItem {
    let project = utf8_percent_encode(&row.project_name, NON_ALPHANUMERIC).to_string();
    let pipeline = utf8_percent_encode(&row.pipeline_name, NON_ALPHANUMERIC).to_string();
    let download_url = if row.kind == "set_entry" {
        (row.artifact_name.is_some() && row.name != ".")
            .then(|| {
                format!(
                    "/api/v1/projects/{project}/pipelines/{pipeline}/builds/{}/artifact-sets/{}/file?path={}",
                    row.build_number,
                    row.item_id,
                    utf8_percent_encode(&row.name, NON_ALPHANUMERIC),
                )
            })
    } else {
        Some(format!(
            "/api/v1/projects/{project}/pipelines/{pipeline}/builds/{}/artifacts/{}",
            row.build_number,
            utf8_percent_encode(&row.name, NON_ALPHANUMERIC),
        ))
    };
    ArtifactRepositoryItem {
        kind: row.kind,
        id: row.item_id,
        name: row.name.clone(),
        set_name: row.set_name,
        path: row.name,
        size: row.size.max(0) as u64,
        sha256: row.sha256,
        executable: row.executable,
        backend: row.backend,
        availability: row.availability,
        created_at: row.created_at,
        source: ArtifactRepositorySource {
            project: row.project_name,
            pipeline: row.pipeline_name,
            build: row.build_number,
            job: row.job_name,
            attempt: row.attempt,
        },
        download_url,
    }
}

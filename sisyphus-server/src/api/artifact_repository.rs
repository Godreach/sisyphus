//! 一级制品库入口状态（票 #122，ADR-0026）：未配置 S3 时明确不可用。

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::ApiError;

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

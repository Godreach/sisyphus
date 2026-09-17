//! 全局 S3 配置只读脱敏与连接自检（票 #122，ADR-0026）。
//! 配置本身来自启动文件/环境，不提供 PUT；凭据永不回显。

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::artifact_repository::S3BackendDto;
use super::error::{ApiError, ErrorBody};
use super::policy::RequireGlobalAdmin;

/// 脱敏的 S3 配置态。
#[derive(Debug, Serialize, ToSchema)]
pub struct S3ConfigState {
    /// 是否已在启动配置中启用并通过校验。
    pub configured: bool,
    /// 非机密字段（未配置时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<S3BackendDto>,
}

/// 单步自检结果。
#[derive(Debug, Serialize, ToSchema)]
pub struct S3TestCheckDto {
    /// 操作名。
    pub op: String,
    /// 是否成功。
    pub ok: bool,
    /// 失败说明（不含凭据）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 管理员显式连接测试报告。
#[derive(Debug, Serialize, ToSchema)]
pub struct S3TestReportDto {
    /// 全部步骤成功。
    pub ok: bool,
    /// 逐步结果。
    pub checks: Vec<S3TestCheckDto>,
}

/// 读全局 S3 配置（脱敏，无凭据）。
#[utoipa::path(
    get,
    path = "/api/v1/config/s3",
    tag = "config",
    responses(
        (status = 200, body = S3ConfigState, description = "脱敏 S3 配置（凭据不回显）"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需全局 admin）", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<S3ConfigState>, ApiError> {
    Ok(Json(match state.s3.as_ref() {
        Some(s3) => {
            let view = s3.public_view();
            S3ConfigState {
                configured: true,
                config: Some(S3BackendDto {
                    endpoint: view.endpoint,
                    region: view.region,
                    bucket: view.bucket,
                    prefix: view.prefix,
                    path_style: view.path_style,
                }),
            }
        }
        None => S3ConfigState {
            configured: false,
            config: None,
        },
    }))
}

/// 管理员显式测试 S3 契约（PUT/HEAD/Range GET/复制/multipart，并清理探针）。
#[utoipa::path(
    post,
    path = "/api/v1/config/s3/test-connection",
    tag = "config",
    responses(
        (status = 200, body = S3TestReportDto, description = "逐步测试结果（失败不泄露凭据）"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需全局 admin）", body = ErrorBody),
        (status = 409, description = "未配置 S3", body = ErrorBody),
    )
)]
pub async fn test_connection(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<S3TestReportDto>, ApiError> {
    let Some(s3) = state.s3.as_ref() else {
        return Err(ApiError::conflict("未配置 S3 后端，无法测试连接"));
    };
    let report = s3.test_connection().await;
    Ok(Json(S3TestReportDto {
        ok: report.ok,
        checks: report
            .checks
            .into_iter()
            .map(|c| S3TestCheckDto {
                op: c.op,
                ok: c.ok,
                detail: c.detail,
            })
            .collect(),
    }))
}

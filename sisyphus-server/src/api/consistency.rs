//! 管理员只读的 SQLite / 对象存储一致性检查（#133）。

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

use super::{AppState, error::ApiError, policy::RequireGlobalAdmin};
use crate::store::ConsistencyReport;

#[allow(missing_docs)]
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct ConsistencyQuery {
    /// 显式启用逐对象 SHA-256；默认只做 stat/HEAD 大小检查。
    pub deep_hash: Option<bool>,
}

/// 执行一次不修改数据的存储一致性检查。
#[utoipa::path(
    get,
    path = "/api/v1/storage/consistency",
    tag = "storage",
    params(ConsistencyQuery),
    responses(
        (status = 200, body = ConsistencyReport, description = "只读一致性检查结果"),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
        (status = 403, description = "仅全局管理员可见", body = super::error::ErrorBody)
    )
)]
pub async fn get(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    Query(query): Query<ConsistencyQuery>,
) -> Result<Json<ConsistencyReport>, ApiError> {
    let report = crate::store::consistency::run(
        &state.pool,
        state.artifacts.root(),
        state.s3.as_deref(),
        query.deep_hash.unwrap_or(false),
    )
    .await?;
    crate::metrics::report_consistency(&report);
    Ok(Json(report))
}

//! 存活探针（ADR-0010 Docker HEALTHCHECK、ADR-0019 不鉴权）。

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// `/healthz` 响应体。
#[derive(Debug, Serialize, ToSchema)]
pub struct Healthz {
    /// 固定 `ok`：仅表进程存活，不代表依赖就绪。
    pub status: &'static str,
}

/// 存活探针：不鉴权、不查库（票 B2a-T3；Spec B2a 裁定本阶段只答进程存活）。
/// ADR-0019 定的深度为「进程 + SQLite `SELECT 1`」，该深度随存储消费与
/// 可观测性批次接入（T4 起池进组合根才有可查对象）。
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "infra",
    responses(
        (status = 200, description = "进程存活", body = Healthz),
    )
)]
pub async fn healthz() -> Json<Healthz> {
    Json(Healthz { status: "ok" })
}

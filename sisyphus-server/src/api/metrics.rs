//! `/metrics` 端点（ADR-0019，票 B5-T7）：Prometheus 文本格式，与业务路由
//! 同端口（8080），默认 PAT 鉴权（`config [metrics] auth = true`，任意登录
//! 角色——运维可为 Prometheus 专建 viewer 用户）；`auth = false` 可关（仅限
//! 可信内网，config 文档注明）。不单开端口。
//!
//! 挂根路由（`GET /metrics`，非 `/api/v1` 下）：Prometheus 抓取惯例路径；
//! 鉴权中间件在 `[metrics] auth` 开启时叠加 [`super::auth::require_auth`]
//! （与业务同缝：Bearer PAT / cookie 会话双通道）。响应 `Content-Type:
//! text/plain; version=0.0.4`。

use axum::http::StatusCode;
use axum::http::header;
use axum::response::{IntoResponse, Response};

/// `/metrics` 文本响应（recorder 未安装时输出空体 200——未装配形态不炸）。
pub async fn get() -> Response {
    let text = crate::metrics::render();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

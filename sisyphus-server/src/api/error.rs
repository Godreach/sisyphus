//! REST 统一 JSON 错误形态：错误码 + message + detail（ADR-0005，Spec B2a §4）。
//!
//! 后续端点一律经 [`ApiError`] 落错误响应；schema 注册在
//! [`crate::api::docs`] 的 OpenAPI 契约里，前端按 `code` 分支、按 `detail`
//! 渲染（如 model 校验错误整组清单）。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// 统一错误响应体。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorBody {
    /// 机器可读错误码（大写蛇形，如 `NOT_FOUND`、`VALIDATION_FAILED`）。
    pub code: String,
    /// 人读错误信息，可直接展示。
    pub message: String,
    /// 结构化补充（如校验错误清单）；无补充时缺省不输出。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// 带 HTTP 状态的 API 错误：端点返回它即落统一 JSON 形态。
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    /// 通用构造。
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        detail: Option<serde_json::Value>,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                code: code.into(),
                message: message.into(),
                detail,
            },
        }
    }

    /// 404：端点不存在（`/api` 前缀未命中的统一兜底）。
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", "端点不存在", None)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

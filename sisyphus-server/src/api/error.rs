//! REST 统一 JSON 错误形态：错误码 + message + detail（ADR-0005，Spec B2a §4）。
//!
//! 后续端点一律经 [`ApiError`] 落错误响应；schema 注册在
//! [`crate::api::docs`] 的 OpenAPI 契约里，前端按 `code` 分支、按 `detail`
//! 渲染（如 model 校验错误整组清单）。store 层错误经 [`From`] 映射进来，
//! handler 用 `?` 即落统一形态。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::ToSchema;

use crate::store::StoreError;

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

/// 校验错误条目：字段定位路径 + 人读信息（与 sisyphus-model 的
/// `ValidationError` 同形态，模型侧类型不透出 API 层）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ValidationIssue {
    /// 字段定位路径（如 `stages[0].jobs[1].steps[0].command`）。
    pub path: String,
    /// 人读错误描述。
    pub message: String,
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

    /// 404：资源存在性（项目、pipeline 等）。
    pub fn resource_not_found(what: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", what, None)
    }

    /// 401：未认证或会话失效（认证中间件全局面统一形态，票 B2b-T1）。
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "未认证或会话已过期",
            None,
        )
    }

    /// 403：已认证但权限不足（授权 extractor 档位判定、全局资源 admin 专属，
    /// 票 B2b-T5）；message 由调用侧给足语境（哪类操作、需要什么档）。
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "FORBIDDEN", message, None)
    }

    /// 422：输入校验失败，错误清单整组透传（Spec B2a §4）。
    pub fn validation(message: impl Into<String>, issues: Vec<ValidationIssue>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_FAILED",
            message,
            Some(serde_json::json!({ "errors": issues })),
        )
    }

    /// 403：CSRF 拒绝（cookie 认证的非安全方法请求跨源/缺同源凭证，
    /// 票 B2b-T2，ADR-0014）。
    pub fn csrf_rejected() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "CSRF_REJECTED",
            "跨站请求被拒绝（CSRF 防护：需同源 Origin 或 Sec-Fetch-Site）",
            None,
        )
    }

    /// 429：登录限流冷却中（票 B2b-T2，ADR-0014）；detail 携带剩余毫秒，
    /// 调用侧再补 `Retry-After` 头。
    pub fn too_many_requests(retry_after_ms: i64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "登录尝试过于频繁，请稍后再试",
            Some(serde_json::json!({ "retry_after_ms": retry_after_ms })),
        )
    }

    /// 409：状态冲突（唯一键、并发写等）。
    pub fn conflict(what: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "CONFLICT", what, None)
    }

    /// 504：上游（Agent 经通道查询）超时——Agent 在线但未在窗口内回响应帧
    /// （票 #76，ADR-0011/0012 列表经通道往返）。
    pub fn gateway_timeout(what: impl Into<String>) -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, "GATEWAY_TIMEOUT", what, None)
    }

    /// 500：内部错误（细节只进日志，不外泄）。
    pub fn internal(context: &str, err: &dyn std::fmt::Display) -> Self {
        tracing::error!(context, error = %err, "API 内部错误");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "服务内部错误，请稍后重试或查看服务端日志",
            None,
        )
    }

    /// HTTP 状态码（测试断言用）。
    pub fn status_code(&self) -> StatusCode {
        self.status
    }

    /// 人读错误信息（CLI 复用 HTTP 校验逻辑后经此取可读消息，避免重复校验
    /// 代码——票 #80，admin create headless 等价）。
    pub(crate) fn message(&self) -> &str {
        &self.body.message
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::InvalidDefinition(errors) => ApiError::validation(
                "Pipeline 定义校验失败",
                errors
                    .into_iter()
                    .map(|e| ValidationIssue {
                        path: e.path,
                        message: e.message,
                    })
                    .collect(),
            ),
            StoreError::NotFound(what) => ApiError::resource_not_found(what),
            StoreError::Unique(what) | StoreError::Conflict(what) => ApiError::conflict(what),
            other => ApiError::internal("store", &other),
        }
    }
}

/// 解析 JSON 请求体：语法/形态错误统一落 422 校验形态（不经 axum 默认的
/// 纯文本拒绝，`/api` 面错误形态全程可被客户端稳定解析）。
pub(crate) fn parse_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        ApiError::validation(
            "请求体不是合法 JSON 或形态不符",
            vec![ValidationIssue {
                path: "$".into(),
                message: e.to_string(),
            }],
        )
    })
}

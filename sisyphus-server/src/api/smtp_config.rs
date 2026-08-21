//! 全局 SMTP 配置端点（票 B5-T5，ADR-0014/0015）。
//!
//! - **GET /api/v1/config/smtp**（全局 admin）：读脱敏——回 `configured` 布尔 +
//!   非机密配置字段 + `password_set` 布尔，密码值永不出 API 面。
//! - **PUT /api/v1/config/smtp**（全局 admin）：写全量——host/port/username/tls
//!   /from_address + 可选 password（`None`=保留旧密码、`Some`=更新、空串=清空），
//!   密码经 XChaCha20-Poly1305 加密落库（复用机密同套，ADR-0015），变更入审计
//!   （`smtp_config_changed`，detail 只记非机密配置 + `password_changed` 布尔，
//!   密码值永不落审计——与机密同纪律）。返回脱敏态（与 GET 同形）。
//!
//! 全局资源、全局 admin 档（ADR-0014）；单行表（id=1），PUT = upsert。

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireGlobalAdmin;
use crate::store::audit::AuditEvent;
use crate::store::now_ms;
use crate::store::smtp_config::{SmtpConfigRow, SmtpTls};

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 脱敏的全局 SMTP 配置（GET 回形 / PUT 回形；密码值不回，回 `password_set` 布尔）。
#[derive(Debug, Serialize, ToSchema)]
pub struct SmtpConfigResponse {
    /// SMTP 主机。
    pub host: String,
    /// SMTP 端口。
    pub port: i64,
    /// SMTP AUTH 用户名（可空——无认证）。
    pub username: Option<String>,
    /// 加密模式。
    pub tls: SmtpTls,
    /// 发件人地址（邮件 From）。
    pub from_address: String,
    /// 是否已设密码（密码值永不回显——只回布尔，ADR-0015 脱敏纪律）。
    pub password_set: bool,
}

impl SmtpConfigResponse {
    /// 由 repo 行转脱敏形态（丢弃 `password_ciphertext`，留 `password_set` 布尔）。
    fn from_row(row: SmtpConfigRow) -> Self {
        Self {
            host: row.host,
            port: row.port,
            username: row.username,
            tls: row.tls,
            from_address: row.from_address,
            password_set: row.password_ciphertext.is_some(),
        }
    }
}

/// GET 响应：是否已配置 + 脱敏配置（未配置时 `config` 省略）。
#[derive(Debug, Serialize, ToSchema)]
pub struct SmtpConfigState {
    /// 是否已配置（单行表有无行）。
    pub configured: bool,
    /// 脱敏配置（未配置时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<SmtpConfigResponse>,
}

/// PUT 请求：全量写。`password` 可选——`None`=保留旧密码、`Some`(非空)=更新、
/// `Some`(空串)=清空。
#[derive(Debug, Deserialize, ToSchema)]
pub struct SmtpConfigRequest {
    /// SMTP 主机。
    pub host: String,
    /// SMTP 端口（1..=65535）。
    pub port: i64,
    /// SMTP AUTH 用户名（可空——无认证）。
    #[serde(default)]
    pub username: Option<String>,
    /// 加密模式（none / starttls / implicit）。
    pub tls: SmtpTls,
    /// 发件人地址（邮件 From）。
    pub from_address: String,
    /// 密码（可选：`None`=保留旧、`Some`=更新/空串清空；值永不回显）。
    #[serde(default)]
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// 读全局 SMTP 配置（脱敏）。
#[utoipa::path(
    get,
    path = "/api/v1/config/smtp",
    tag = "config",
    responses(
        (status = 200, body = SmtpConfigState, description = "全局 SMTP 配置脱敏态（未配置时 configured=false、config 省略）"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需全局 admin）", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<SmtpConfigState>, ApiError> {
    let config = state
        .smtp_config
        .get()
        .await?
        .map(SmtpConfigResponse::from_row);
    Ok(Json(SmtpConfigState {
        configured: config.is_some(),
        config,
    }))
}

/// 写全局 SMTP 配置（全量；密码加密落库 + 变更入审计）。
#[utoipa::path(
    put,
    path = "/api/v1/config/smtp",
    tag = "config",
    request_body = SmtpConfigRequest,
    responses(
        (status = 200, body = SmtpConfigState, description = "已写入，回脱敏态"),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需全局 admin）", body = ErrorBody),
        (status = 422, description = "输入校验失败", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    body: Bytes,
) -> Result<Json<SmtpConfigState>, ApiError> {
    let req: SmtpConfigRequest = parse_body(&body)?;
    validate(&req)?;

    // 密码语义：None=保留旧、Some(非空)=更新、Some(空)=清空。保留旧需读回现有密文。
    let existing = state.smtp_config.get().await?;
    let password_changed = req.password.is_some();
    let ciphertext: Option<Vec<u8>> = match req.password.as_deref() {
        Some(p) if !p.is_empty() => Some(
            crate::secrets::encrypt(&state.master_key, p.as_bytes())
                .map_err(|e| ApiError::internal("smtp password encrypt", &e))?,
        ),
        // 空串 = 清空（落 NULL）。
        Some(_) => None,
        // None = 保留旧密文（未配置则为 None）。
        None => existing.and_then(|r| r.password_ciphertext),
    };

    state
        .smtp_config
        .set(
            &req.host,
            req.port,
            req.username.as_deref(),
            ciphertext.as_deref(),
            req.tls,
            &req.from_address,
            &auth.username,
            now_ms(),
        )
        .await?;

    // 审计（ADR-0015）：全局配置变更是审计事件。detail 只记非机密配置字段 +
    // `password_changed` 布尔；密码值永不落审计（与机密同纪律），username 视作
    // 凭据邻近、不记（与 SCM 凭据只记 set/clear 动作同保守取）。
    state
        .audit
        .insert(
            now_ms(),
            &auth.username,
            AuditEvent::SmtpConfigChanged,
            None,
            Some(
                &serde_json::json!({
                    "host": req.host,
                    "port": req.port,
                    "tls": req.tls.as_str(),
                    "from_address": req.from_address,
                    "password_changed": password_changed,
                })
                .to_string(),
            ),
        )
        .await?;

    // 回脱敏态（由刚写入的值构造——权威且免再读）。
    Ok(Json(SmtpConfigState {
        configured: true,
        config: Some(SmtpConfigResponse {
            host: req.host,
            port: req.port,
            username: req.username,
            tls: req.tls,
            from_address: req.from_address,
            password_set: ciphertext.is_some(),
        }),
    }))
}

/// 输入校验：host/from 非空、port 合法区间。`tls` 经 serde 枚举反序列化已校验
/// （非法值由 `parse_body` 落 422）。
fn validate(req: &SmtpConfigRequest) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if req.host.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "host".into(),
            message: "SMTP 主机不能为空".into(),
        });
    }
    if !(1..=65535).contains(&req.port) {
        issues.push(ValidationIssue {
            path: "port".into(),
            message: "端口须在 1..=65535".into(),
        });
    }
    if req.from_address.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "from_address".into(),
            message: "发件人地址不能为空".into(),
        });
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("SMTP 配置校验失败", issues))
    }
}

//! 审计查询端点（票 B2b-T7，ADR-0015）：`GET /api/v1/audit`。
//!
//! 仅全局 admin（[`super::policy::RequireGlobalAdmin`] 声明）：其他角色一律
//! 403（审计是全局安全回放面，v1 不给项目 admin 项目域视图，ADR-0015）。
//! 按时间 / 用户 / 项目 / 事件类型过滤 + 分页，时间倒序（新事件在前）。
//!
//! 过滤参数全部可选（query string；非法事件类型 422、非法分页参数 422，
//! 不落任何查询）。事件类型取值域与 store 层 [`AuditEvent`] 契约同源
//! （`as_str()` 即 API 参数值），OpenAPI 参数带 enum——审计页的过滤下拉
//! 按契约渲染。

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue};
use super::policy::RequireGlobalAdmin;
use crate::store::audit::{AuditEvent, AuditQuery};

/// 分页上限：单页最大条数（防拖全表；页大小由调用侧在 limit 内自选）。
pub const AUDIT_PAGE_MAX: i64 = 200;

/// `event` 过滤参数的 OpenAPI schema：取值域与 store 层 [`AuditEvent`] 契约
/// 同源（enum 由 [`AuditEvent::ALL`] 生成，新增事件类型自动入契约）。
fn audit_event_schema() -> utoipa::openapi::schema::Object {
    utoipa::openapi::schema::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::Type::String)
        .enum_values(Some(AuditEvent::ALL.iter().map(|e| e.as_str())))
        .build()
}

/// 审计查询参数（全部可选，AND 组合；分页 `limit`/`offset`，缺省
/// limit=50、offset=0 由解析层统一收口）。
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct AuditParams {
    /// 时间下限（含；Unix 毫秒）。
    pub since: Option<i64>,
    /// 时间上限（含；Unix 毫秒）。
    pub until: Option<i64>,
    /// 操作人（用户名，精确匹配）。
    pub user: Option<String>,
    /// 项目名（精确匹配）。
    pub project: Option<String>,
    /// 事件类型（取值域见 enum：`login_success`、`secret_created` 等）。
    #[param(schema_with = audit_event_schema)]
    pub event: Option<String>,
    /// 单页条数（1..=200，默认 50）。
    pub limit: Option<i64>,
    /// 跳过条数（默认 0）。
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 审计条目（detail 为 JSON 对象——机密事件只含名，永不出现值形态）。
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditEntryResponse {
    /// 行 id。
    pub id: i64,
    /// 事件时间（Unix 毫秒）。
    pub ts: i64,
    /// 操作人（用户名）。
    pub actor: String,
    /// 事件类型（[`AuditEvent`] 契约值）。
    pub event: String,
    /// 项目名（可空：非项目域事件）。
    pub project: Option<String>,
    /// 结构化补充（可空：机密名 / 目标用户 / 成员角色清单等）。
    pub detail: Option<serde_json::Value>,
}

/// 审计回放（全局 admin）：按时间 / 用户 / 项目 / 事件类型过滤 + 分页，
/// 时间倒序（新事件在前）。
#[utoipa::path(
    get,
    path = "/api/v1/audit",
    tag = "audit",
    params(AuditParams),
    responses(
        (status = 200, description = "审计条目（时间倒序；detail 为 JSON 对象，机密事件只记名）", body = [AuditEntryResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员（审计仅全局 admin 可读）", body = ErrorBody),
        (status = 422, description = "过滤/分页参数非法（未知事件类型、limit 越界等）", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    Query(params): Query<AuditParams>,
) -> Result<Json<Vec<AuditEntryResponse>>, ApiError> {
    let filter = parse_filter(&params)?;
    let (limit, offset) = parse_paging(&params)?;

    let entries = state.audit.query(&filter, limit, offset).await?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let detail = match entry.detail.as_deref() {
            Some(raw) => Some(
                serde_json::from_str(raw)
                    .map_err(|e| ApiError::internal("audit detail parse", &e))?,
            ),
            None => None,
        };
        out.push(AuditEntryResponse {
            id: entry.id,
            ts: entry.ts,
            actor: entry.actor,
            event: entry.event,
            project: entry.project_name,
            detail,
        });
    }
    Ok(Json(out))
}

/// 过滤条件解析：时间/用户/项目原样透传；事件类型经 [`AuditEvent::parse`]
/// 校验（未知值 422——过滤值域与契约同源，不静默放宽）。空条件即全量。
fn parse_filter(params: &AuditParams) -> Result<AuditQuery, ApiError> {
    let event = match &params.event {
        Some(raw) => match AuditEvent::parse(raw) {
            Some(_) => Some(raw.clone()),
            None => {
                return Err(ApiError::validation(
                    "审计过滤参数非法",
                    vec![ValidationIssue {
                        path: "event".into(),
                        message: format!("未知事件类型：{raw}（取值域见 OpenAPI enum）"),
                    }],
                ));
            }
        },
        None => None,
    };
    Ok(AuditQuery {
        since: params.since,
        until: params.until,
        user: params
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(ToOwned::to_owned),
        project: params
            .project
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(ToOwned::to_owned),
        event,
    })
}

/// 分页解析：limit 默认 50、1..=200；offset 默认 0、非负。非法即 422。
fn parse_paging(params: &AuditParams) -> Result<(i64, i64), ApiError> {
    let mut issues = Vec::new();
    let limit = params.limit.unwrap_or(50);
    if !(1..=AUDIT_PAGE_MAX).contains(&limit) {
        issues.push(ValidationIssue {
            path: "limit".into(),
            message: format!("limit 须在 1..={AUDIT_PAGE_MAX} 之间"),
        });
    }
    let offset = params.offset.unwrap_or(0);
    if offset < 0 {
        issues.push(ValidationIssue {
            path: "offset".into(),
            message: "offset 不能为负".into(),
        });
    }
    if issues.is_empty() {
        Ok((limit, offset))
    } else {
        Err(ApiError::validation("审计分页参数非法", issues))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn paging_defaults_and_bounds() {
        assert_eq!(
            parse_paging(&AuditParams::default()).expect("默认"),
            (50, 0)
        );
        assert_eq!(
            parse_paging(&AuditParams {
                limit: Some(1),
                offset: Some(0),
                ..Default::default()
            })
            .expect("下界"),
            (1, 0)
        );
        assert_eq!(
            parse_paging(&AuditParams {
                limit: Some(AUDIT_PAGE_MAX),
                offset: Some(10),
                ..Default::default()
            })
            .expect("上界"),
            (AUDIT_PAGE_MAX, 10)
        );

        // limit 越界 / offset 为负：422。
        for limit in [0, -1, AUDIT_PAGE_MAX + 1] {
            let err = parse_paging(&AuditParams {
                limit: Some(limit),
                offset: None,
                ..Default::default()
            })
            .unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{limit}"
            );
        }
        let err = parse_paging(&AuditParams {
            offset: Some(-1),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn filter_accepts_known_event_and_rejects_unknown() {
        // 已知事件类型：透传（值经 parse 校验同源）。
        let ok = parse_filter(&AuditParams {
            event: Some("secret_created".into()),
            ..Default::default()
        })
        .expect("已知事件应过");
        assert_eq!(ok.event.as_deref(), Some("secret_created"));

        // 未知事件：422（过滤值域不放宽）。
        for raw in ["", "login", "build_started", "secret_value_read"] {
            let err = parse_filter(&AuditParams {
                event: Some(raw.into()),
                ..Default::default()
            })
            .unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{raw:?} 应 422"
            );
        }

        // 空白 user/project：按无过滤对待（空串等价未提供）。
        let f = parse_filter(&AuditParams {
            user: Some("   ".into()),
            project: Some("".into()),
            ..Default::default()
        })
        .expect("空白应放行");
        assert_eq!(f.user, None);
        assert_eq!(f.project, None);
    }
}

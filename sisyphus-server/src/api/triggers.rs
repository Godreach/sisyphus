//! 触发器 REST 端点（票 B2c-T6，ADR-0016）：cron / poll 触发源 CRUD（项目
//! admin 档）。消费 [`crate::trigger`] 的 spec 解析/校验与 [`crate::store::triggers`]
//! repo；触发源本身（cron 扫表 + poll 轮询）由 [`crate::trigger::TriggerEngine`]
//! 后台驱动，本模块只做配置面。
//!
//! - **授权**：项目 admin 档（[`Permission::Manage`]，[`RequireAdmin`]）。
//!   runner 档 403、无角色项目 404（与不存在同形，授权 extractor 先裁决不泄
//!   存在性，ADR-0014）。
//! - **建**（`POST`）：`kind` 选 cron / poll（同 pipeline 各一）；cron 给
//!   `{"expr":"..."}`（5 字段），poll 给 `{"interval_minutes":N}`（缺省取
//!   config `[triggers] poll_interval_minutes`）。同 (pipeline, kind) 已存在
//!   → 409。pipeline 不存在 → 404。
//! - **改**（`PATCH`）：改 spec 与启停；spec 必须匹配路径 `{kind}`（不匹配
//!   的 spec → 422）。poll 启用（false→true）重置基线——下次探测记当前 head
//!   作基线、不触发（ADR-0016「启用时记基线不触发」）；cron 启用仅置位
//!   （tick 以 `last_probe_at` 锚点续跑，停用期间命中点不补跑多份）。
//! - **删除**：本批不做（触发器删除对构建历史影响随项目删一并裁定）。
//!
//! 触发历史经构建的 `trigger_detail` + builds 行呈现，不单独建表（Spec B2c §2）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireAdmin;
use crate::store::triggers::{TriggerInput, TriggerKind, TriggerRow};
use crate::trigger::{CronSpec, PollSpec, SpecError};

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 触发器类型（cron / poll；manual 不建行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKindDto {
    /// cron 定时触发（5 字段表达式）。
    Cron,
    /// poll SCM 轮询（基线 + commit-id 去重）。
    Poll,
}

impl TriggerKindDto {
    /// 从路径段解析；未知值视为资源不存在（404 由调用侧落）。
    fn parse(segment: &str) -> Option<Self> {
        match segment {
            "cron" => Some(Self::Cron),
            "poll" => Some(Self::Poll),
            _ => None,
        }
    }

    fn to_store(self) -> TriggerKind {
        match self {
            Self::Cron => TriggerKind::Cron,
            Self::Poll => TriggerKind::Poll,
        }
    }
}

/// cron spec（输入 / 输出同型：5 字段表达式）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CronSpecDto {
    /// 5 字段 cron 表达式（`分 时 日 月 周`，标准 Unix/CI 形态）。
    pub expr: String,
}

/// poll spec 输入：`interval_minutes` 缺省取 config `[triggers] poll_interval_minutes`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PollSpecInputDto {
    /// 轮询节奏（分钟，>= 1；缺省取 config 默认 5 分钟）。
    #[serde(default)]
    pub interval_minutes: Option<i64>,
}

/// 建触发器请求体：`kind` 选 cron / poll；spec 按 kind 给（不匹配者忽略）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTriggerRequest {
    /// 触发器类型。
    pub kind: TriggerKindDto,
    /// 启停（缺省 true：创建即启用，首探记基线不触发）。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// cron spec（kind=cron 时必填）。
    #[serde(default)]
    pub cron: Option<CronSpecDto>,
    /// poll spec（kind=poll 时必填；`interval_minutes` 缺省取 config 默认）。
    #[serde(default)]
    pub poll: Option<PollSpecInputDto>,
}

/// 改触发器请求体：字段均可选，只更新出现者；spec 必须匹配路径 `{kind}`。
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct PatchTriggerRequest {
    /// 启停。poll 启用（false→true）重置基线（ADR-0016）。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// cron spec（路径 kind=cron 时生效；kind=poll 时给则 422）。
    #[serde(default)]
    pub cron: Option<CronSpecDto>,
    /// poll spec（路径 kind=poll 时生效；kind=cron 时给则 422）。
    #[serde(default)]
    pub poll: Option<PollSpecInputDto>,
}

/// 触发器响应：配置 + 探测/基线状态（触发历史经 builds 行呈现，不在此回）。
#[derive(Debug, Serialize, ToSchema)]
pub struct TriggerResponse {
    /// 触发器类型。
    pub kind: TriggerKindDto,
    /// 配置（`{"expr":"..."}` / `{"interval_minutes":N}`，落库原文）。
    pub spec: serde_json::Value,
    /// 启停。
    pub enabled: bool,
    /// poll 基线 commit（创建/启用时记、不触发；cron 恒空）。
    pub baseline_commit: Option<String>,
    /// 最近探测/命中时间（Unix 毫秒；cron 为最近命中点）。
    pub last_probe_at: Option<i64>,
    /// 最近探测/触发失败（失败记入、按节奏重试、不自动禁用）。
    pub last_probe_error: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// 触发器清单（项目 admin 档）：cron / poll 各一，按类型序。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/triggers",
    tag = "triggers",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
    ),
    responses(
        (status = 200, description = "触发器清单（cron/poll 各一）", body = [TriggerResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（触发器管理需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, pipeline)): Path<(String, String)>,
) -> Result<Json<Vec<TriggerResponse>>, ApiError> {
    let rows = state
        .triggers
        .list_by_pipeline(access.project.id, &pipeline)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(to_response(row));
    }
    Ok(Json(out))
}

/// 建触发器（项目 admin 档）：cron / poll 各一；同 (pipeline, kind) 已存在 409。
#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/triggers",
    tag = "triggers",
    request_body = CreateTriggerRequest,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
    ),
    responses(
        (status = 201, description = "已创建", body = TriggerResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（建触发器需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或 pipeline 不存在", body = ErrorBody),
        (status = 409, description = "同 (pipeline, kind) 触发器已存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（spec 缺失/非法、kind 未知）", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, pipeline)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<TriggerResponse>), ApiError> {
    let req: CreateTriggerRequest = parse_body(&body)?;
    // pipeline 须存在（触发器绑定 pipeline；不存在即 404，不建孤儿触发器）。
    if state
        .pipelines
        .get(&access.project.name, &pipeline)
        .await?
        .is_none()
    {
        return Err(ApiError::resource_not_found(format!(
            "pipeline {pipeline} 不存在"
        )));
    }
    let (kind, spec) = build_spec_for_create(&state, &req)?;
    let enabled = req.enabled.unwrap_or(true);
    let row = state
        .triggers
        .create(TriggerInput {
            project_id: access.project.id,
            pipeline_name: pipeline,
            kind,
            spec,
            enabled,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(to_response(row))))
}

/// 触发器详情（项目 admin 档）：按 {kind} 取一个。
#[utoipa::path(
    get,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/triggers/{kind}",
    tag = "triggers",
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("kind" = String, Path, description = "触发器类型（cron/poll）"),
    ),
    responses(
        (status = 200, description = "触发器详情", body = TriggerResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或触发器不存在", body = ErrorBody),
    )
)]
pub async fn get_one(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, pipeline, kind_str)): Path<(String, String, String)>,
) -> Result<Json<TriggerResponse>, ApiError> {
    let Some(kind) = TriggerKindDto::parse(&kind_str) else {
        return Err(ApiError::resource_not_found(format!(
            "触发器类型 {kind_str} 不存在（取值：cron/poll）"
        )));
    };
    let row = load_one(&state, &access.project.id, &pipeline, kind).await?;
    Ok(Json(to_response(row)))
}

/// 改触发器（项目 admin 档）：改 spec 与启停。poll 启用重置基线（ADR-0016）。
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{name}/pipelines/{pipeline}/triggers/{kind}",
    tag = "triggers",
    request_body = PatchTriggerRequest,
    params(
        ("name" = String, Path, description = "项目名"),
        ("pipeline" = String, Path, description = "pipeline 名"),
        ("kind" = String, Path, description = "触发器类型（cron/poll）"),
    ),
    responses(
        (status = 200, description = "已更新", body = TriggerResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "权限不足（需项目 admin 档）", body = ErrorBody),
        (status = 404, description = "项目不存在/不可见，或触发器不存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（spec 非法、spec 与 kind 不匹配）", body = ErrorBody),
    )
)]
pub async fn patch(
    State(state): State<AppState>,
    RequireAdmin(access): RequireAdmin,
    Path((_project, pipeline, kind_str)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<Json<TriggerResponse>, ApiError> {
    let req: PatchTriggerRequest = parse_body(&body)?;
    let Some(kind) = TriggerKindDto::parse(&kind_str) else {
        return Err(ApiError::resource_not_found(format!(
            "触发器类型 {kind_str} 不存在（取值：cron/poll）"
        )));
    };
    let row = load_one(&state, &access.project.id, &pipeline, kind).await?;
    // spec 必须匹配路径 kind（不匹配 → 422，防误改）。
    let new_spec = build_spec_for_patch(&state, kind, &req)?;
    // 启停：落地后判定 false→true 过渡（poll 重置基线，ADR-0016）。
    let old_enabled = row.enabled;
    let new_enabled = req.enabled.unwrap_or(old_enabled);
    // update 返回是否命中行；行刚由 load_one 确认存在，此处只看错误。
    state
        .triggers
        .update(row.id, new_spec.as_deref(), req.enabled)
        .await?;
    if !old_enabled && new_enabled && kind == TriggerKindDto::Poll {
        // poll 启用：清基线 → 下次探测记当前 head 作基线、不触发（禁用期间
        // 落地的提交随基线一并吸收）。
        state.triggers.reset_baseline(row.id).await?;
    }
    let fresh = state
        .triggers
        .get(row.id)
        .await?
        .expect("刚更新的触发器必存在");
    Ok(Json(to_response(fresh)))
}

// ---------------------------------------------------------------------------
// 组装辅助
// ---------------------------------------------------------------------------

/// 按 (project, pipeline, kind) 取触发器；不存在 404。
async fn load_one(
    state: &AppState,
    project_id: &i64,
    pipeline: &str,
    kind: TriggerKindDto,
) -> Result<TriggerRow, ApiError> {
    state
        .triggers
        .get_by_key(*project_id, pipeline, kind.to_store())
        .await?
        .ok_or_else(|| {
            ApiError::resource_not_found(format!(
                "触发器 {}/{} 不存在",
                pipeline,
                kind_as_str(kind)
            ))
        })
}

/// 构造建触发器的 (kind, spec)：按 req.kind 取对应 spec、校验、序列化。
/// spec 缺失或非法 → 422；不匹配 kind 的 spec 字段忽略（建时不报错）。
fn build_spec_for_create(
    state: &AppState,
    req: &CreateTriggerRequest,
) -> Result<(TriggerKind, String), ApiError> {
    match req.kind {
        TriggerKindDto::Cron => {
            let Some(cron) = &req.cron else {
                return Err(missing_spec("cron", "cron"));
            };
            let spec = CronSpec {
                expr: cron.expr.clone(),
            };
            validate_cron_spec(&spec)?;
            Ok((TriggerKind::Cron, spec.to_json()))
        }
        TriggerKindDto::Poll => {
            let interval = req
                .poll
                .as_ref()
                .and_then(|p| p.interval_minutes)
                .unwrap_or(state.poll_interval_minutes);
            let spec = PollSpec {
                interval_minutes: interval,
            };
            validate_poll_spec(&spec)?;
            Ok((TriggerKind::Poll, spec.to_json()))
        }
    }
}

/// 构造改触发器的新 spec（None 表示不改）：spec 必须匹配路径 kind，不匹配
/// 的 spec 字段出现即 422（防误改）。
fn build_spec_for_patch(
    state: &AppState,
    kind: TriggerKindDto,
    req: &PatchTriggerRequest,
) -> Result<Option<String>, ApiError> {
    match kind {
        TriggerKindDto::Cron => {
            if req.poll.is_some() {
                return Err(spec_kind_mismatch("poll", "cron"));
            }
            let Some(cron) = &req.cron else {
                return Ok(None);
            };
            let spec = CronSpec {
                expr: cron.expr.clone(),
            };
            validate_cron_spec(&spec)?;
            Ok(Some(spec.to_json()))
        }
        TriggerKindDto::Poll => {
            if req.cron.is_some() {
                return Err(spec_kind_mismatch("cron", "poll"));
            }
            let Some(poll) = &req.poll else {
                return Ok(None);
            };
            let interval = poll.interval_minutes.unwrap_or(state.poll_interval_minutes);
            let spec = PollSpec {
                interval_minutes: interval,
            };
            validate_poll_spec(&spec)?;
            Ok(Some(spec.to_json()))
        }
    }
}

/// cron spec 校验：5 字段 + cron crate 可解析（复用 trigger 模块单点）。
fn validate_cron_spec(spec: &CronSpec) -> Result<(), ApiError> {
    spec.validate().map_err(spec_error_to_api)
}

/// poll spec 校验：节奏 >= 1 分钟（复用 trigger 模块单点）。
fn validate_poll_spec(spec: &PollSpec) -> Result<(), ApiError> {
    spec.validate().map_err(spec_error_to_api)
}

/// SpecError → 422 校验形态。
fn spec_error_to_api(e: SpecError) -> ApiError {
    let (path, message) = match &e {
        SpecError::Json(m) => ("spec", m.clone()),
        SpecError::Cron(m) => ("spec.expr", m.clone()),
        SpecError::Interval(m) => ("spec.interval_minutes", m.clone()),
    };
    ApiError::validation(
        "触发器 spec 非法",
        vec![ValidationIssue {
            path: path.into(),
            message,
        }],
    )
}

fn missing_spec(kind: &str, field: &str) -> ApiError {
    ApiError::validation(
        "触发器 spec 缺失",
        vec![ValidationIssue {
            path: field.into(),
            message: format!("{kind} 触发器需提供 {field} spec"),
        }],
    )
}

fn spec_kind_mismatch(given: &str, expected: &str) -> ApiError {
    ApiError::validation(
        "触发器 spec 与类型不匹配",
        vec![ValidationIssue {
            path: given.into(),
            message: format!("{expected} 触发器不接受 {given} spec"),
        }],
    )
}

fn kind_as_str(kind: TriggerKindDto) -> &'static str {
    match kind {
        TriggerKindDto::Cron => "cron",
        TriggerKindDto::Poll => "poll",
    }
}

/// 触发器行 → 响应（spec 落库原文解析为 JSON；坏 JSON 退化为 null 不 500）。
fn to_response(row: TriggerRow) -> TriggerResponse {
    let spec = serde_json::from_str(&row.spec).unwrap_or(serde_json::Value::Null);
    TriggerResponse {
        kind: match row.kind {
            TriggerKind::Cron => TriggerKindDto::Cron,
            TriggerKind::Poll => TriggerKindDto::Poll,
        },
        spec,
        enabled: row.enabled,
        baseline_commit: row.baseline_commit,
        last_probe_at: row.last_probe_at,
        last_probe_error: row.last_probe_error,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

// 内联单测：spec 构造/校验映射（纯逻辑，不发请求）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MasterKey;

    async fn state_with_poll_default(default: i64) -> AppState {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        let state = AppState::new(pool, false, MasterKey::generate(), default);
        // AppState 不持有 dir；存进静态槽防过早清理（目录活到进程退出）。
        LEAK.lock().unwrap().push(dir);
        state
    }

    static LEAK: std::sync::Mutex<Vec<tempfile::TempDir>> = std::sync::Mutex::new(Vec::new());

    use axum::body::Bytes;

    fn cron_req(expr: &str) -> CreateTriggerRequest {
        CreateTriggerRequest {
            kind: TriggerKindDto::Cron,
            enabled: Some(true),
            cron: Some(CronSpecDto { expr: expr.into() }),
            poll: None,
        }
    }

    fn poll_req(interval: Option<i64>) -> CreateTriggerRequest {
        CreateTriggerRequest {
            kind: TriggerKindDto::Poll,
            enabled: Some(true),
            cron: None,
            poll: Some(PollSpecInputDto {
                interval_minutes: interval,
            }),
        }
    }

    #[tokio::test]
    async fn build_spec_for_create_cron_validates() {
        let state = state_with_poll_default(5).await;
        let (kind, spec) =
            build_spec_for_create(&state, &cron_req("0 2 * * *")).expect("合法 cron");
        assert_eq!(kind, TriggerKind::Cron);
        assert!(spec.contains("0 2 * * *"));
        // 坏 cron → 422。
        let err = build_spec_for_create(&state, &cron_req("0 2 * *")).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn build_spec_for_create_poll_defaults_interval_from_config() {
        let state = state_with_poll_default(7).await;
        // poll spec 不给 interval → 取 config 默认 7。
        let req = CreateTriggerRequest {
            kind: TriggerKindDto::Poll,
            enabled: Some(true),
            cron: None,
            poll: Some(PollSpecInputDto {
                interval_minutes: None,
            }),
        };
        let (kind, spec) = build_spec_for_create(&state, &req).expect("合法 poll");
        assert_eq!(kind, TriggerKind::Poll);
        assert!(spec.contains("7"), "缺省取 config 默认：{spec}");
        // 显式 interval 覆盖默认。
        let (.., spec) = build_spec_for_create(&state, &poll_req(Some(3))).expect("显式");
        assert!(spec.contains("3"));
        // interval < 1 → 422。
        let err = build_spec_for_create(&state, &poll_req(Some(0))).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn build_spec_for_create_missing_spec_is_422() {
        let state = state_with_poll_default(5).await;
        // cron 不给 cron spec → 422。
        let req = CreateTriggerRequest {
            kind: TriggerKindDto::Cron,
            enabled: Some(true),
            cron: None,
            poll: None,
        };
        let err = build_spec_for_create(&state, &req).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        // poll 不给 poll spec → 仍可（interval 取默认）；给空 poll 也行。
        let req = CreateTriggerRequest {
            kind: TriggerKindDto::Poll,
            enabled: Some(true),
            cron: None,
            poll: None,
        };
        // poll spec 缺失：interval 取 config 默认（建 poll 不强制给 spec）。
        let (kind, ..) = build_spec_for_create(&state, &req).expect("poll spec 可缺省");
        assert_eq!(kind, TriggerKind::Poll);
    }

    #[tokio::test]
    async fn build_spec_for_patch_rejects_mismatched_spec() {
        let state = state_with_poll_default(5).await;
        // 路径 kind=cron，body 给 poll spec → 422。
        let req = PatchTriggerRequest {
            enabled: None,
            cron: None,
            poll: Some(PollSpecInputDto {
                interval_minutes: Some(5),
            }),
        };
        let err = build_spec_for_patch(&state, TriggerKindDto::Cron, &req).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        // 路径 kind=poll，body 给 cron spec → 422。
        let req = PatchTriggerRequest {
            enabled: None,
            cron: Some(CronSpecDto {
                expr: "0 2 * * *".into(),
            }),
            poll: None,
        };
        let err = build_spec_for_patch(&state, TriggerKindDto::Poll, &req).unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        // 匹配的 spec 合法 → Some。
        let req = PatchTriggerRequest {
            enabled: None,
            cron: Some(CronSpecDto {
                expr: "0 6 * * *".into(),
            }),
            poll: None,
        };
        let spec = build_spec_for_patch(&state, TriggerKindDto::Cron, &req)
            .expect("合法")
            .expect("应 Some");
        assert!(spec.contains("0 6 * * *"));
        // 不改 spec → None。
        let req = PatchTriggerRequest::default();
        assert!(
            build_spec_for_patch(&state, TriggerKindDto::Cron, &req)
                .expect("空 patch")
                .is_none()
        );
    }

    #[test]
    fn trigger_kind_dto_parses_known_segments() {
        assert_eq!(TriggerKindDto::parse("cron"), Some(TriggerKindDto::Cron));
        assert_eq!(TriggerKindDto::parse("poll"), Some(TriggerKindDto::Poll));
        assert_eq!(TriggerKindDto::parse("manual"), None);
        assert_eq!(TriggerKindDto::parse("bogus"), None);
    }

    // 消除未使用告警（Bytes 在端点用，单测不直接用）。
    #[allow(dead_code)]
    fn _silence() -> Bytes {
        Bytes::new()
    }
}

//! Agent 注册面端点（票 B2c-T3/T4、B3-T2，ADR-0008 最小注册面 + Spec B3 §7）：
//! 全局 admin 专属（除注册码兑 token 的公开端点）。
//!
//! - **建条目**（`POST /agents`）：签发 per-Agent token（`sisa_` 族，复用
//!   B2b token 基座：SHA-256 落库、明文只在创建响应出现一次）+ 一次性
//!   注册码（明文只在创建响应出现一次，库里只存哈希；ADR-0010 一次性 +
//!   24h 有效期，建条目即签）。
//! - **注册码兑 token**（`POST /agent/register`，**公开**——Agent 凭注册码
//!   换长期 token，票 #57）：校验注册码（哈希匹配 404 / 一次性未用 409 /
//!   短有效期 403 / Agent 未停用 403）→ 签发新 token 换旧（旧明文不可
//!   找回，「换」由重新签发兑现）+ 注册码置已用。
//! - **列表 / 详情**：在线状态 / 系统与自定义标签 / 槽位数 / 磁盘占用
//!   （ADR-0019：卷级/缓存/工作区采样，随心跳上报入库）。详情含槽位占用
//!   现状（在途任务数，ADR-0008 中心化计数）。
//! - **启停 / 编辑**（`PATCH /agents/{name}`）：停用即踢线（认证面与在线
//!   面一律不命中——下一连接/下一帧即拒）；改 `max_concurrency` /
//!   `custom_labels`。
//!
//! 权限：管理面全局 admin（[`super::policy::RequireGlobalAdmin`]）——Agent
//! 是全局资源，管理面不按项目分域（与用户管理同档）。系统标签不可手编：
//! 由注册/心跳上报（gRPC 面），本模块只读不回写。
//!
//! 审计（ADR-0015）：Agent 建立 + 注册码签发（`agent_created`）、注册码兑
//! token（`agent_registered`，actor 记 Agent 名）、停用即吊销 token
//! （`agent_disabled`）、启用恢复（`agent_enabled`）——detail 记构建机名，
//! token/注册码值永不落审计（与 PAT 同纪律）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ErrorBody, ValidationIssue, parse_body};
use super::policy::RequireGlobalAdmin;
use crate::auth::{TokenFamily, generate_register_code, generate_token, token_hash};
use crate::store::StoreError;
use crate::store::agents::{AgentRow, NewAgent, VolumeUsage};
use crate::store::audit::AuditEvent;
use crate::store::jobs::JobRepo;
use crate::store::now_ms;

/// token/注册码哈希撞唯一约束的换值重试次数（32 字节随机碰撞，概率上不可达）。
const TOKEN_ATTEMPTS: usize = 3;

/// 注册码有效期（ADR-0010：一次性 + 24h 过期；过期/用掉后管理员可重新生成）。
/// 建条目即签 24h（Unix 毫秒），注册端点兑码时校验。
pub const REGISTER_CODE_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// 建 Agent 条目请求体（全局 admin）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAgentRequest {
    /// 构建机名（唯一；1-64 位字母数字或 `_ . -`，trim 后生效——名会进
    /// URL 路径 `/agents/{name}`）。
    pub name: String,
    /// 自定义标签（key=value 字符串数组，可空/省略 = 空集；管理员可编辑，
    /// 匹配与系统标签取并集做 AND 全集语义）。
    #[serde(default)]
    pub custom_labels: Option<Vec<String>>,
    /// 并发槽位数（默认 1；>= 1）。
    #[serde(default)]
    pub max_concurrency: Option<i32>,
}

/// 启停 / 编辑请求体（全局 admin）：字段均可选，只更新出现者。
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchAgentRequest {
    /// 停用（true）即踢线：认证面与在线面即刻不命中，运行中任务处置归
    /// sched（B2c-T4）；false = 启用恢复。
    #[serde(default)]
    pub disabled: Option<bool>,
    /// 并发槽位数（>= 1）。
    #[serde(default)]
    pub max_concurrency: Option<i32>,
    /// 自定义标签整组替换（key=value 字符串数组）。
    #[serde(default)]
    pub custom_labels: Option<Vec<String>>,
}

/// Agent 管理视图（无 token/注册码值形态——明文只在创建响应出现一次）。
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentResponse {
    /// 构建机名（唯一）。
    pub name: String,
    /// 在线状态（心跳 15s 收、45s 无心跳判离线）。
    pub online: bool,
    /// 停用标志（停用即踢线）。
    pub disabled: bool,
    /// 系统事实标签（sisyphus/os、sisyphus/arch、sisyphus/container；由
    /// 注册/心跳上报，不可手编）。
    pub system_labels: Vec<String>,
    /// 自定义标签（管理员可编辑）。
    pub custom_labels: Vec<String>,
    /// 并发槽位数。
    pub max_concurrency: i32,
    /// 在途任务数（running/unknown 占槽，ADR-0008 中心化计数）。
    pub active_jobs: i64,
    /// 最近心跳时间（Unix 毫秒；从未在线为空）。
    pub last_seen_at: Option<i64>,
    /// 磁盘占用（ADR-0019：卷级/缓存/工作区最近采样；从未上报为空）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_usage: Option<DiskUsageDto>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 卷级磁盘占用（详情视图）。
#[derive(Debug, Serialize, ToSchema)]
pub struct VolumeUsageDto {
    /// 挂载点/盘符。
    pub mount_point: String,
    /// 总量（字节）。
    pub total_bytes: i64,
    /// 剩余量（字节）。
    pub free_bytes: i64,
}

/// 磁盘占用视图（ADR-0019：随心跳上报入库）。
#[derive(Debug, Serialize, ToSchema)]
pub struct DiskUsageDto {
    /// 卷级剩余/总量（多卷逐项）。
    pub volumes: Vec<VolumeUsageDto>,
    /// 缓存占用（记账值）。
    pub cache_bytes: i64,
    /// 工作区占用最近采样。
    pub workspace_bytes: i64,
}

/// 建条目响应：token 与注册码明文仅此一次返回（此后任何端点不再出现值；
/// 库里只存各自哈希，任何端点都无法找回）。
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedAgentResponse {
    /// per-Agent 通道 token（`sisa_` + 43 字符）。请立即保存：本响应是
    /// 唯一一次出现。
    pub token: String,
    /// 一次性注册码（`sisa_reg_` + 43 字符；注册码换 token 流程随 Agent
    /// 批次，本批建条目即签发 token）。
    pub register_code: String,
    /// Agent 管理视图。
    pub agent: AgentResponse,
}

/// 注册码换 token 请求体（票 #57，Spec B3 §7）：Agent 启动凭 `--reg-key`
/// 兑码，body 带构建机名 + 注册码（注册码哈希查行定位 Agent——一次性、
/// 24h 短有效期、未停用才放行）。
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterAgentRequest {
    /// 构建机名（与建条目时一致；仅用于核对/审计，不参与寻径）。
    pub name: String,
    /// 一次性注册码（`sisa_reg_` + 43 字符，建条目响应签发）。
    pub register_code: String,
}

/// 注册码兑 token 响应：per-Agent 通道 token（`sisa_` + 43 字符）。
///
/// 兑码即换新（库里只存哈希、旧明文不可找回，兑码时重新签发并吊销旧
/// token）——注册码换 token 的「换」语义由此兑现；此后 Agent 以此 token
/// 直连，注册码置已用。
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterAgentResponse {
    /// per-Agent 通道 token（`sisa_` + 43 字符）。请立即保存：注册码兑完
    /// 即作废，本响应是 token 唯一一次出现（此后直连不再需要注册码）。
    pub token: String,
}

/// 建 Agent 条目（全局 admin）：签发 token + 注册码，落库行（离线、未
/// 停用；系统标签空——由首次连接上报）。
#[utoipa::path(
    post,
    path = "/api/v1/agents",
    tag = "agents",
    request_body = CreateAgentRequest,
    responses(
        (status = 201, description = "已创建；token 与注册码明文仅此一次返回", body = CreatedAgentResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 409, description = "Agent 名已存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（名非空/字符集、标签形态、槽位 >= 1）", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    body: Bytes,
) -> Result<(StatusCode, Json<CreatedAgentResponse>), ApiError> {
    let req: CreateAgentRequest = parse_body(&body)?;
    let name = req.name.trim();
    validate_create(&req)?;

    let custom_labels = serde_json::to_string(&req.custom_labels.unwrap_or_default())
        .expect("标签 JSON 序列化恒可成功（纯字符串）");
    let max_concurrency = req.max_concurrency.unwrap_or(1);

    // 生成 → 哈希落库；撞唯一约束重试。名称冲突是确定性输入（重试不会
    // 自愈），先以「名是否已存在」判定：存在 → 409；否则是 token/注册码
    // 哈希碰撞（32 字节随机，概率上不可达）→ 换值重试。
    for _ in 0..TOKEN_ATTEMPTS {
        let token = generate_token(TokenFamily::Agent);
        let register_code = generate_register_code();
        let agent = state
            .agents
            .create(NewAgent {
                name: name.into(),
                token_hash: token_hash(&token),
                system_labels: "[]".into(),
                custom_labels: custom_labels.clone(),
                max_concurrency,
                register_code_hash: token_hash(&register_code),
                register_code_expires_at: now_ms() + REGISTER_CODE_TTL_MS,
            })
            .await;
        match agent {
            Ok(agent) => {
                // 审计（ADR-0015 清单：Agent 建立 + 注册码签发）——detail
                // 记构建机名，token/注册码值永不落审计（与 PAT 同纪律）。
                state
                    .audit
                    .insert(
                        now_ms(),
                        &auth.username,
                        AuditEvent::AgentCreated,
                        None,
                        Some(&serde_json::json!({ "agent": agent.name }).to_string()),
                    )
                    .await?;
                return Ok((
                    StatusCode::CREATED,
                    Json(CreatedAgentResponse {
                        token,
                        register_code,
                        agent: to_response(&state, agent).await?,
                    }),
                ));
            }
            Err(StoreError::Unique(_)) => {
                if state
                    .agents
                    .get_by_name(name)
                    .await?
                    .is_some()
                {
                    return Err(ApiError::conflict(format!("Agent 名已存在：{name}")));
                }
                continue; // 哈希碰撞，换值重试
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(ApiError::internal(
        "agent create",
        &"连续撞唯一约束，随机源疑似异常",
    ))
}

/// 注册码换 token（票 #57，Spec B3 §7 唯一动 server 的面）：Agent 凭
/// `--reg-key` 兑码换长期 token。**公开端点**（不经认证中间件——Agent
/// 此刻只有注册码没有 token，与 login 同档放行）。
///
/// 校验序（票 #57 AC）：注册码哈希匹配（无效 404）→ 一次性未用（已用
/// 409）→ 短有效期（过期 403）→ Agent 未停用（停用 403）→ 兑码：签发
/// 新 token 换旧（库里只存哈希、旧明文不可找回，「换」由重新签发兑现，
/// 顺带吊销任何外泄的旧 token）+ 注册码置已用。兑码原子闸在 store 层
/// （`redeem_register_code` 条件更新，并发双换只成一个）。
#[utoipa::path(
    post,
    path = "/api/v1/agent/register",
    tag = "agents",
    request_body = RegisterAgentRequest,
    responses(
        (status = 200, description = "已兑码：返回 per-Agent token（sisa_ + 43 字符；注册码置已用、token 换新）", body = RegisterAgentResponse),
        (status = 403, description = "注册码已过期，或 Agent 已停用", body = ErrorBody),
        (status = 404, description = "注册码无效（哈希无匹配）", body = ErrorBody),
        (status = 409, description = "注册码已使用（一次性）", body = ErrorBody),
        (status = 422, description = "输入校验失败（name/register_code 非空）", body = ErrorBody),
    )
)]
pub async fn register(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<RegisterAgentResponse>, ApiError> {
    let req: RegisterAgentRequest = parse_body(&body)?;
    validate_register(&req)?;

    // 哈希查行：无效注册码 404（与「行不存在」不区分，不借错误形态泄露）。
    let agent = state
        .agents
        .find_by_register_code(&token_hash(&req.register_code))
        .await?
        .ok_or_else(|| ApiError::resource_not_found("注册码无效"))?;

    // 一次性：已用 409（含并发双换的败者——另一请求已兑）。
    if agent.register_code_used {
        return Err(ApiError::conflict("注册码已使用（一次性，兑完作废）"));
    }
    // 短有效期：过期 403（ADR-0010：一次性 + 24h 过期；迁移前旧行为空 =
    // 不失效的遗留语义）。
    if let Some(expires_at) = agent.register_code_expires_at
        && now_ms() > expires_at
    {
        return Err(ApiError::forbidden("注册码已过期，请让管理员重新生成"));
    }
    // 停用即踢线：停用 Agent 不配发 token。
    if agent.disabled {
        return Err(ApiError::forbidden("Agent 已停用，无法注册"));
    }

    // 兑码：签发新 token 换旧 + 置已用（原子闸，条件含未停用 + 未过期——
    // 防读后写前的 TOCTOU）。name 仅核对/审计——寻径以注册码哈希为准
    // （注册码本身即高熵随机，name 不参与匹配）。
    let now = now_ms();
    let token = generate_token(TokenFamily::Agent);
    let ok = state
        .agents
        .redeem_register_code(agent.id, &token_hash(&token), now)
        .await?;
    if !ok {
        // 原子闸失败：并发被抢 / 兑码瞬间被停用 / 过期——读到的状态已过期，
        // 按「已使用/已不可兑」兜底（被抢 409，停用/过期本就该拒）。
        return Err(ApiError::conflict("注册码已使用（一次性，兑完作废）"));
    }

    // 审计（ADR-0015：注册码兑 token 入账）——actor 记 Agent 名（非认证
    // 用户；detail 记构建机名，token/注册码值永不落审计）。
    state
        .audit
        .insert(
            now_ms(),
            &agent.name,
            AuditEvent::AgentRegistered,
            None,
            Some(&serde_json::json!({ "agent": agent.name }).to_string()),
        )
        .await?;

    tracing::info!(agent = %agent.name, "agent 凭注册码兑 token 成功");
    Ok(Json(RegisterAgentResponse { token }))
}

/// 注册码兑 token 校验：name / register_code 非空（trim 后；注册码形态
/// 不在此验——哈希无匹配即 404，形态脏与无效同判）。
fn validate_register(req: &RegisterAgentRequest) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if req.name.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "name".into(),
            message: "构建机名不能为空".into(),
        });
    }
    if req.register_code.trim().is_empty() {
        issues.push(ValidationIssue {
            path: "register_code".into(),
            message: "注册码不能为空".into(),
        });
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("注册输入校验失败", issues))
    }
}

/// Agent 清单（全局 admin；按名排序，含已停用，无凭据值形态）。
#[utoipa::path(
    get,
    path = "/api/v1/agents",
    tag = "agents",
    responses(
        (status = 200, description = "全部 Agent（按名排序，含已停用）", body = [AgentResponse]),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
) -> Result<Json<Vec<AgentResponse>>, ApiError> {
    let rows = state.agents.list().await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(to_response(&state, row).await?);
    }
    Ok(Json(out))
}

/// Agent 详情（全局 admin）：在线/标签/槽位/磁盘占用。
#[utoipa::path(
    get,
    path = "/api/v1/agents/{name}",
    tag = "agents",
    params(("name" = String, Path, description = "构建机名")),
    responses(
        (status = 200, description = "Agent 详情（含在途任务数/磁盘占用）", body = AgentResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 404, description = "Agent 不存在", body = ErrorBody),
    )
)]
pub async fn get_one(
    State(state): State<AppState>,
    RequireGlobalAdmin(_auth): RequireGlobalAdmin,
    Path(name): Path<String>,
) -> Result<Json<AgentResponse>, ApiError> {
    let row = state
        .agents
        .get_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("Agent {name} 不存在")))?;
    Ok(Json(to_response(&state, row).await?))
}

/// 启停 / 编辑（全局 admin）：字段均可选，只更新出现者。停用即踢线——
/// 认证面（下一连接/下一帧）与在线面（心跳不生效）即刻不命中。
#[utoipa::path(
    patch,
    path = "/api/v1/agents/{name}",
    tag = "agents",
    request_body = PatchAgentRequest,
    params(("name" = String, Path, description = "构建机名")),
    responses(
        (status = 200, description = "已更新，返回落定后的 Agent", body = AgentResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 403, description = "非全局管理员", body = ErrorBody),
        (status = 404, description = "Agent 不存在", body = ErrorBody),
        (status = 422, description = "输入校验失败（标签形态、槽位 >= 1）", body = ErrorBody),
    )
)]
pub async fn patch(
    State(state): State<AppState>,
    RequireGlobalAdmin(auth): RequireGlobalAdmin,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<AgentResponse>, ApiError> {
    let req: PatchAgentRequest = parse_body(&body)?;
    validate_patch(&req)?;

    let row = state
        .agents
        .get_by_name(&name)
        .await?
        .ok_or_else(|| ApiError::resource_not_found(format!("Agent {name} 不存在")))?;

    if let Some(disabled) = req.disabled {
        state
            .agents
            .set_disabled(row.id, disabled)
            .await?;
        // 审计（ADR-0015：token 吊销/恢复——停用即吊销踢线）——detail 记
        // 构建机名（值永不落审计）。
        state
            .audit
            .insert(
                now_ms(),
                &auth.username,
                if disabled {
                    AuditEvent::AgentDisabled
                } else {
                    AuditEvent::AgentEnabled
                },
                None,
                Some(&serde_json::json!({ "agent": row.name }).to_string()),
            )
            .await?;
    }
    if req.max_concurrency.is_some() || req.custom_labels.is_some() {
        let max_concurrency = req.max_concurrency.unwrap_or(row.max_concurrency);
        let custom_labels = match &req.custom_labels {
            Some(labels) => serde_json::to_string(labels)
                .expect("标签 JSON 序列化恒可成功（纯字符串）"),
            None => row.custom_labels.clone(),
        };
        state
            .agents
            .update_spec(row.id, max_concurrency, &custom_labels)
            .await?;
    }

    let updated = state
        .agents
        .get(row.id)
        .await?
        .expect("刚更新的行必存在");
    Ok(Json(to_response(&state, updated).await?))
}

/// 组装管理视图：并发读在途任务数（槽位占用，ADR-0008 中心化计数）与
/// 磁盘占用解析（脏 JSON 视为库损坏）。
async fn to_response(state: &AppState, row: AgentRow) -> Result<AgentResponse, ApiError> {
    let active_jobs = JobRepo::new(state.pool.clone())
        .active_by_agent(row.id)
        .await?;
    let disk_usage = row.disk_usage()?.map(|u| DiskUsageDto {
        volumes: u
            .volumes
            .into_iter()
            .map(|v: VolumeUsage| VolumeUsageDto {
                mount_point: v.mount_point,
                total_bytes: v.total_bytes,
                free_bytes: v.free_bytes,
            })
            .collect(),
        cache_bytes: u.cache_bytes,
        workspace_bytes: u.workspace_bytes,
    });
    Ok(AgentResponse {
        name: row.name,
        online: row.online,
        disabled: row.disabled,
        system_labels: serde_json::from_str(&row.system_labels)
            .map_err(StoreError::DefinitionJson)?,
        custom_labels: serde_json::from_str(&row.custom_labels)
            .map_err(StoreError::DefinitionJson)?,
        max_concurrency: row.max_concurrency,
        active_jobs,
        last_seen_at: row.last_seen_at,
        disk_usage,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// 建条目校验：名非空 + 字符集/长度（进 URL 路径 `/agents/{name}`，与
/// 用户同纪律）、标签形态（key=value）、槽位 >= 1。
fn validate_create(req: &CreateAgentRequest) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if let Some(issue) = agent_name_issue(&req.name) {
        issues.push(issue);
    }
    if let Some(issue) = concurrency_issue(req.max_concurrency) {
        issues.push(issue);
    }
    if let Some(issue) = labels_issue(req.custom_labels.as_deref()) {
        issues.push(issue);
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("Agent 输入校验失败", issues))
    }
}

/// 编辑校验：只校验出现者（缺省字段不参与——PATCH 语义）。
fn validate_patch(req: &PatchAgentRequest) -> Result<(), ApiError> {
    let mut issues = Vec::new();
    if let Some(issue) = concurrency_issue(req.max_concurrency) {
        issues.push(issue);
    }
    if let Some(issue) = labels_issue(req.custom_labels.as_deref()) {
        issues.push(issue);
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation("Agent 输入校验失败", issues))
    }
}

/// 构建机名问题项：trim 后非空、1..=64 字符、限字母数字与 `_ . -`（与
/// 用户名同纪律——名会进 URL 路径）。
fn agent_name_issue(name: &str) -> Option<ValidationIssue> {
    let trimmed = name.trim();
    let charset_ok = !trimmed.is_empty()
        && trimmed.chars().count() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    (!charset_ok).then(|| ValidationIssue {
        path: "name".into(),
        message: "构建机名须为 1-64 位字母数字或 _ . -".into(),
    })
}

/// 槽位问题项：>= 1（0 槽的 Agent 永远接不到任务，建出来即废）。
fn concurrency_issue(max_concurrency: Option<i32>) -> Option<ValidationIssue> {
    max_concurrency.is_some_and(|v| v < 1).then(|| ValidationIssue {
        path: "max_concurrency".into(),
        message: "并发槽位须 >= 1".into(),
    })
}

/// 自定义标签问题项：每个标签须为 `key=value` 形态（键与值都非空、键无
/// 空白——AND 全集匹配的输入面，形态脏了匹配语义全乱）。
fn labels_issue(labels: Option<&[String]>) -> Option<ValidationIssue> {
    let labels = labels?;
    let bad = labels.iter().find(|label| {
        let Some((raw_key, raw_value)) = label.split_once('=') else {
            return true;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        key.is_empty() || value.is_empty() || raw_key.contains(char::is_whitespace)
    });
    bad.map(|label| ValidationIssue {
        path: "custom_labels".into(),
        message: format!("标签须为 key=value 形态且键/值非空：{label:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_create_enforces_name_labels_and_concurrency() {
        let ok = CreateAgentRequest {
            name: "linux-1".into(),
            custom_labels: Some(vec!["region=cn".into()]),
            max_concurrency: Some(2),
        };
        assert!(validate_create(&ok).is_ok());

        // 名空白 / 超长 / 非法字符：422。
        for name in ["   ", &"x".repeat(65), "a/b"] {
            let err = validate_create(&CreateAgentRequest {
                name: name.into(),
                custom_labels: None,
                max_concurrency: None,
            })
            .unwrap_err();
            assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY, "{name}");
        }

        // 槽位 0 / 负：422。
        for v in [0, -1] {
            let err = validate_create(&CreateAgentRequest {
                name: "linux-1".into(),
                custom_labels: None,
                max_concurrency: Some(v),
            })
            .unwrap_err();
            assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        // 标签形态：缺 =、空键、空值、键含空格：422。
        for label in ["region", "=cn", "region=", "region =cn"] {
            let err = validate_create(&CreateAgentRequest {
                name: "linux-1".into(),
                custom_labels: Some(vec![label.into()]),
                max_concurrency: None,
            })
            .unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{label}"
            );
        }
    }

    #[test]
    fn validate_patch_checks_only_present_fields() {
        // PATCH 缺省字段不参与校验（只改 disabled 不带标签/槽位也合法）。
        assert!(validate_patch(&PatchAgentRequest {
            disabled: Some(true),
            max_concurrency: None,
            custom_labels: None,
        })
        .is_ok());

        assert!(validate_patch(&PatchAgentRequest {
            disabled: None,
            max_concurrency: Some(0),
            custom_labels: None,
        })
        .unwrap_err()
        .status_code()
            == StatusCode::UNPROCESSABLE_ENTITY);
    }
}

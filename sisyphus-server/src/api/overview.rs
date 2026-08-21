//! 概览快照端点（ADR-0019，票 B5-T7）：`GET /api/v1/overview`——stat 卡
//! 全量真值 + 三类事实警示态 + 最近构建，喂 web 概览页。
//!
//! - **可见性**：任意登录角色可读（普通轮询面；Agent/队列/槽位/磁盘占用
//!   是全局运行态，不涉项目数据泄露——概览页本就对全体登录用户开放）。
//!   最近构建按调用者项目可见性过滤（全局 admin 全量、普通用户仅成员项目）。
//! - **双消费**：响应值即 [`crate::snapshot::compute`] 的 DB 真值；调用本
//!   端点同时把同一份数经 [`crate::metrics::report_snapshot`] 灌入 recorder
//!   （`/metrics` 读同一 recorder）——「同一份计数喂两路」在数据落点兑现。
//! - 单点失败整体 500（DB 故障时 UI 已有 loadError 降级面，概览是低负载
//!   轮询面，不做部分成功）。

use axum::Json;
use axum::extract::State;
use utoipa::ToSchema;

use super::AppState;
use super::auth::AuthContext;
use super::error::{ApiError, ErrorBody};
use crate::snapshot;

/// `GET /api/v1/overview` 响应体（stat 卡 + 警示态 + 最近构建；ADR-0019）。
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct OverviewResponse {
    /// 队列深度（全部 queued 任务）。
    pub queue_depth: u64,
    /// 队列深度原因分类（保序；`uncategorized` 为未标注者）。
    pub queue_reasons: Vec<QueueReason>,
    /// Agent 在线数。
    pub agents_online: u64,
    /// Agent 总数（含停用）。
    pub agents_total: u64,
    /// 槽位占用（running/unknown 在途任务）。
    pub slots_used: u64,
    /// 槽位总量（在线 Agent max_concurrency 之和）。
    pub slots_total: u64,
    /// 构建终态计数。
    pub builds_terminal: BuildsTerminalCounts,
    /// 产物字节占用。
    pub artifact_bytes: u64,
    /// 日志字节占用（压缩体）。
    pub log_bytes: u64,
    /// 事实型警示态（零阈值）。
    pub alerts: Alerts,
    /// 最近构建（跨可见项目，按最近活动倒序）。
    pub recent_builds: Vec<RecentBuild>,
}

/// 队列深度原因分类条目。
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct QueueReason {
    /// 原因标签（missing_labels / no_online_agent / no_slot / uncategorized）。
    pub reason: String,
    /// 该原因下的等待任务数。
    pub depth: u64,
}

/// 构建终态计数（四态）。
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct BuildsTerminalCounts {
    /// 成功。
    pub succeeded: u64,
    /// 失败。
    pub failed: u64,
    /// 取消。
    pub cancelled: u64,
    /// 超时。
    pub timeout: u64,
}

/// 事实型警示态（true 即有、false 即无；零阈值配置，ADR-0019）。
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct Alerts {
    /// 存在无匹配 Agent 的任务。
    pub has_no_match: bool,
    /// 存在启用但离线的 Agent。
    pub has_offline_agent: bool,
    /// 存在排空或版本不兼容的 Agent。
    pub has_draining_incompatible: bool,
}

/// 最近构建条目（概览页列表）。
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct RecentBuild {
    /// 项目名。
    pub project: String,
    /// pipeline 名。
    pub pipeline: String,
    /// per-pipeline 构建号。
    pub number: i64,
    /// 构建状态（queued/running/succeeded/failed/cancelled/timeout）。
    pub status: String,
    /// 触发源（manual/cron/poll）。
    pub trigger: String,
    /// 开始时刻（Unix 毫秒；未运行 null）。
    pub started_at: Option<i64>,
    /// 终态时刻（Unix 毫秒）。
    pub finished_at: Option<i64>,
}

/// 概览快照：stat 卡全量真值 + 三类警示态 + 最近构建。任意登录角色。
#[utoipa::path(
    get,
    path = "/api/v1/overview",
    tag = "overview",
    responses(
        (status = 200, description = "概览快照（stat 卡 + 警示态 + 最近构建）", body = OverviewResponse),
        (status = 401, description = "未认证", body = ErrorBody),
        (status = 500, description = "聚合失败（DB 故障）", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<OverviewResponse>, ApiError> {
    // 可见项目（最近构建过滤面；与 /projects 同可见性语义）。
    let visible = state
        .projects
        .list_visible(auth.is_admin, auth.user_id)
        .await?;
    let snap = snapshot::compute(&state.pool).await?;
    let recent =
        snapshot::recent_builds(&state.pool, &visible, snapshot::RECENT_BUILDS_LIMIT).await?;

    // 同一份数灌入 recorder（/metrics 双消费，ADR-0019）。
    crate::metrics::report_snapshot(&snap);

    Ok(Json(OverviewResponse {
        queue_depth: snap.queue_depth(),
        queue_reasons: snap
            .queue
            .iter()
            .map(|(reason, depth)| QueueReason {
                reason: (*reason).to_string(),
                depth: *depth,
            })
            .collect(),
        agents_online: snap.agents_online,
        agents_total: snap.agents_total,
        slots_used: snap.slots_used,
        slots_total: snap.slots_total,
        builds_terminal: BuildsTerminalCounts {
            succeeded: snap.builds_terminal.get("succeeded").copied().unwrap_or(0),
            failed: snap.builds_terminal.get("failed").copied().unwrap_or(0),
            cancelled: snap.builds_terminal.get("cancelled").copied().unwrap_or(0),
            timeout: snap.builds_terminal.get("timeout").copied().unwrap_or(0),
        },
        artifact_bytes: snap.artifact_bytes,
        log_bytes: snap.log_bytes,
        alerts: Alerts {
            has_no_match: snap.has_no_match,
            has_offline_agent: snap.has_offline_agent,
            has_draining_incompatible: snap.has_draining_incompatible,
        },
        recent_builds: recent
            .into_iter()
            .map(|b| RecentBuild {
                project: b.project,
                pipeline: b.pipeline,
                number: b.number,
                status: b.status.to_string(),
                trigger: b.trigger.to_string(),
                started_at: b.started_at,
                finished_at: b.finished_at,
            })
            .collect(),
    }))
}

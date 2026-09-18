//! Agent 日志缓冲快照：持久保留最后一次可用信息，供调度和管理界面共用。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use super::StoreError;

/// 最近心跳上报的缓冲压力及待归档量。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct LogBufferReport {
    /// 实际缓冲字节。
    pub bytes: i64,
    /// 本机容量配置。
    pub capacity_bytes: i64,
    /// 待确认 attempt 数。
    pub pending_archives: i64,
    /// 压力或采样故障时不可派发。
    pub pressured: bool,
    /// 最近可用的错误。
    pub last_error: Option<String>,
    /// 最后收到报告的 Server 时刻。
    pub reported_at: i64,
}

/// 更新心跳快照，不改变执行结果。
pub async fn report(
    pool: &SqlitePool,
    agent: i64,
    usage: &LogBufferReport,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO agent_log_buffers (agent_id, bytes, capacity_bytes, pending_archives, pressured, last_error, reported_at)
        VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(agent_id) DO UPDATE SET bytes=excluded.bytes,
        capacity_bytes=excluded.capacity_bytes, pending_archives=excluded.pending_archives,
        pressured=excluded.pressured, last_error=excluded.last_error, reported_at=excluded.reported_at")
        .bind(agent).bind(usage.bytes).bind(usage.capacity_bytes).bind(usage.pending_archives)
        .bind(usage.pressured).bind(&usage.last_error).bind(usage.reported_at).execute(pool).await?;
    Ok(())
}

/// 即使 Agent 离线仍返回最后可用信息。
pub async fn latest(pool: &SqlitePool, agent: i64) -> Result<Option<LogBufferReport>, StoreError> {
    Ok(sqlx::query_as(
        "SELECT bytes, capacity_bytes, pending_archives, pressured, last_error, reported_at
        FROM agent_log_buffers WHERE agent_id=?",
    )
    .bind(agent)
    .fetch_optional(pool)
    .await?)
}

//! 任务 repo（票 B2c-T1，Spec B2c §2，ADR-0006/0008）：调度状态底座的 jobs 面。
//!
//! 行以 (build_id, stage_index, name, attempt) 唯一：重跑同任务占新行
//! attempt+1，已成功任务的行与结果保留（ADR-0006 重跑语义）。状态全集合
//! 经 [`JobRepo::transition`] 条件更新（终态吸收）：unknown 为离线不判死
//! 中间态（Agent 侧继续跑，重连回归 running）；宽限超时转 failed；Agent
//! 重启丢任务报 aborted（ADR-0008）。sched 的「同任务新 attempt」经
//! [`Self::next_attempt`] 创建。槽位占用计数经 [`Self::active_by_agent`]
//!（running/unknown 在途任务占槽，下发到任务终态）。

use sqlx::SqlitePool;

use super::StoreError;

/// 任务状态（ADR-0006 失败/重试语义 + ADR-0008 离线/超时处置；
/// 落库文本 `as_str()` 为契约值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// 已入 pending 池等待调度（全局 FIFO）。
    Queued,
    /// 已下发且 Agent 确认在跑。
    Running,
    /// 步骤全部成功。
    Succeeded,
    /// 失败（非 allow_failure 即触发 fail-fast 级联）。
    Failed,
    /// 构建取消/同阶段失败级联。
    Cancelled,
    /// 阶段 when 不满足整级跳过 / 级联跳过后续阶段。
    Skipped,
    /// 离线不判死中间态：Agent 侧继续跑，重连回归 running。
    Unknown,
    /// 超时走取消路径的终态。
    Timeout,
    /// Agent 重启丢任务上报的终态（判失败、走 fail-fast）。
    Aborted,
}

impl JobStatus {
    /// 落库文本（schema 取值域）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
        }
    }

    /// 从落库文本解析（未知值视为库损坏）。
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            "unknown" => Ok(Self::Unknown),
            "timeout" => Ok(Self::Timeout),
            "aborted" => Ok(Self::Aborted),
            other => Err(StoreError::Db(sqlx::Error::ColumnDecode {
                index: "jobs.status".into(),
                source: format!("未知 jobs.status：{other}").into(),
            })),
        }
    }

    /// 终态集合：queued/running/unknown 之外皆吸收（succeeded/failed/
    /// cancelled/skipped/timeout/aborted 不可再迁移，ADR-0006 全集合）。
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running | Self::Unknown)
    }

    /// 是否失败类终态（fail-fast 的触发面：failed/timeout/aborted；
    /// allow_failure 豁免由调用侧按行裁决）。engine 终态裁决与自动重试
    /// 面共用此谓词。
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::Timeout | Self::Aborted)
    }

    /// 是否占用 Agent 槽位（在途任务：下发 ack 到任务终态，ADR-0008）。
    pub fn occupies_slot(self) -> bool {
        matches!(self, Self::Running | Self::Unknown)
    }
}

/// 任务行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// 行 id。
    pub id: i64,
    /// 属主构建 id。
    pub build_id: i64,
    /// 阶段序号（从 0 起；快照内阶段数组的下标）。
    pub stage_index: i32,
    /// 任务名。
    pub name: String,
    /// 任务状态。
    pub status: JobStatus,
    /// 第几次执行（同任务重跑 attempt+1）。
    pub attempt: i32,
    /// 组装好的 ResolvedJobSpec JSON 快照（审计「当时下发什么」）。
    pub spec_json: Option<String>,
    /// 调度到的 Agent 行 id（未调度为空）。
    pub agent_id: Option<i64>,
    /// 已匹配回显的标签（JSON 数组，key=value 字符串）。
    pub labels: String,
    /// 任务超时（分钟，0 = 无限）。
    pub timeout_minutes: i32,
    /// 自动重试次数（耗尽仍失败才算失败）。
    pub retry_count: i32,
    /// allow_failure 豁免 fail-fast。
    pub allow_failure: bool,
    /// 开始时刻（Unix 毫秒）。
    pub started_at: Option<i64>,
    /// 终态时刻（Unix 毫秒）。
    pub finished_at: Option<i64>,
    /// 退出码（可空）。
    pub exit_code: Option<i32>,
    /// 详情（失败原因、缺失机密名、超时等）。
    pub detail: Option<String>,
    /// 判离线转 unknown 的时刻（Unix 毫秒；orphan 宽限计时起点，重连回归
    /// running 清空，ADR-0008）。
    pub unknown_at: Option<i64>,
    /// pending 池等待原因（缺失标签 / 等待上线 / 等待槽位；匹配下发清空）。
    pub waiting_detail: Option<String>,
}

/// 新建任务输入（初始 queued、attempt 由调用侧定——重跑语义的裁决方）。
#[derive(Debug, Clone)]
pub struct NewJob {
    /// 属主构建 id。
    pub build_id: i64,
    /// 阶段序号。
    pub stage_index: i32,
    /// 任务名。
    pub name: String,
    /// 第几次执行（首跑 1；重跑 attempt+1）。
    pub attempt: i32,
    /// ResolvedJobSpec JSON 快照（可空）。
    pub spec_json: Option<String>,
    /// 调度到的 Agent 行 id（可空）。
    pub agent_id: Option<i64>,
    /// 已匹配回显标签（JSON 数组文本）。
    pub labels: Vec<String>,
    /// 任务超时（分钟）。
    pub timeout_minutes: i32,
    /// 自动重试次数。
    pub retry_count: i32,
    /// allow_failure 豁免。
    pub allow_failure: bool,
}

/// 任务 repo：新建 / 全集合状态迁移 / attempt 重跑 / 槽位与待跑查询。
#[derive(Debug, Clone)]
pub struct JobRepo {
    pool: SqlitePool,
}

impl JobRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 新建任务行（初始 queued，无时间戳）。`labels` 序列化为 JSON 数组。
    pub async fn insert(&self, input: NewJob) -> Result<JobRow, StoreError> {
        let labels = serde_json::to_string(&input.labels).map_err(StoreError::DefinitionJson)?;
        let result = sqlx::query(
            "INSERT INTO jobs
                (build_id, stage_index, name, status, attempt, spec_json, agent_id,
                 labels, timeout_minutes, retry_count, allow_failure)
             VALUES (?, ?, ?, 'queued', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(input.build_id)
        .bind(input.stage_index)
        .bind(&input.name)
        .bind(input.attempt)
        .bind(&input.spec_json)
        .bind(input.agent_id)
        .bind(labels)
        .bind(input.timeout_minutes)
        .bind(input.retry_count)
        .bind(input.allow_failure)
        .execute(&self.pool)
        .await?;
        self.get(result.last_insert_rowid())
            .await
            .map(|row| row.expect("刚插入的行必存在"))
    }

    /// 状态迁移（条件更新，终态吸收）：queued/running/unknown → 目标状态。
    ///
    /// 只有进 running 才记 `started_at`（首次运行时刻，重连回归 running
    /// 保留首启时刻）；排队中直接取消/跳过的任务不落开始时刻（Spec B2c
    /// §2：started_at 即开始执行时刻）。进终态记 `finished_at`；
    /// `exit_code`/`detail` 只在 Some 时覆写（保留历史信息，未知值不清）。
    /// 进 running 清 `unknown_at`（重连回归即结束宽限计时）与
    /// `waiting_detail`（下发即结束等待）；进 unknown 记 `unknown_at`（宽限
    /// 计时起点）。已是终态或行不存在返回 `false`。
    pub async fn transition(
        &self,
        id: i64,
        to: JobStatus,
        exit_code: Option<i32>,
        detail: Option<&str>,
        now: i64,
    ) -> Result<bool, StoreError> {
        let finished = to.is_terminal().then_some(now);
        let started = (to == JobStatus::Running).then_some(now);
        let result = sqlx::query(
            "UPDATE jobs
             SET status = ?, started_at = COALESCE(started_at, ?),
                 finished_at = ?,
                 exit_code = COALESCE(?, exit_code),
                 detail = COALESCE(?, detail),
                 unknown_at = CASE
                     WHEN ? = 'unknown' THEN ?
                     WHEN ? = 'running' THEN NULL
                     ELSE unknown_at END,
                 waiting_detail = CASE WHEN ? = 'running' THEN NULL ELSE waiting_detail END
             WHERE id = ? AND status IN ('queued', 'running', 'unknown')",
        )
        .bind(to.as_str())
        .bind(started)
        .bind(finished)
        .bind(exit_code)
        .bind(detail)
        .bind(to.as_str())
        .bind(now)
        .bind(to.as_str())
        .bind(to.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 同任务的下一 attempt 新行（ADR-0006 重跑语义：失败任务起继续，
    /// 已成功任务保留）：从当前行复制任务语义字段（spec/labels/超时/重试/
    /// allow_failure），attempt+1 新建 queued 行，返回新行。
    ///
    /// 当前行必须是终态失败类（failed/timeout/aborted——cancelled/skipped
    /// 是外部取消语义，不属重跑面）；非终态返回 `None`，任务不存在返回
    /// [`StoreError::NotFound`]。
    pub async fn next_attempt(&self, id: i64) -> Result<Option<JobRow>, StoreError> {
        let row = self
            .get(id)
            .await?
            .ok_or_else(|| StoreError::NotFound("任务不存在".into()))?;
        if !row.status.is_failure() {
            return Ok(None);
        }
        let next = self
            .insert(NewJob {
                build_id: row.build_id,
                stage_index: row.stage_index,
                name: row.name,
                attempt: row.attempt + 1,
                spec_json: row.spec_json,
                agent_id: None,
                labels: serde_json::from_str(&row.labels)
                    .map_err(StoreError::DefinitionJson)?,
                timeout_minutes: row.timeout_minutes,
                retry_count: row.retry_count,
                allow_failure: row.allow_failure,
            })
            .await?;
        Ok(Some(next))
    }

    /// 为任务补写 spec 快照（engine 组装完成后落库，审计「当时下发什么」）。
    /// 任务必须仍 queued（已终态不再补写）；返回 false 表示行不存在或已终态。
    pub async fn set_spec(&self, id: i64, spec_json: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE jobs SET spec_json = ? WHERE id = ? AND status = 'queued'")
            .bind(spec_json)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 某 Agent 当前在途任务数（running/unknown 占槽，ADR-0008）。
    pub async fn active_by_agent(&self, agent_id: i64) -> Result<i64, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE agent_id = ? AND status IN ('running', 'unknown')",
        )
        .bind(agent_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// 构建下待入池任务（queued，按阶段/名排序输出稳定——sched 取就绪集）。
    pub async fn queued_by_build(&self, build_id: i64) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE build_id = ? AND status = 'queued' ORDER BY stage_index, id",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// 构建下全部任务（构建详情视图：阶段/任务状态与耗时）。
    pub async fn list_by_build(&self, build_id: i64) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE build_id = ? ORDER BY stage_index, id",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// 按行 id 取任务；不存在返回 `None`。
    pub async fn get(&self, id: i64) -> Result<Option<JobRow>, StoreError> {
        let row = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(JobRow::from_tuple).transpose()
    }

    // -----------------------------------------------------------------------
    // sched 面（票 B2c-T4，ADR-0008）：pending 池 / 在途下发 / 超时 / 宽限 /
    // 取消。所有调度状态都落库（无内存队列），重启从库重建。
    // -----------------------------------------------------------------------

    /// 全局 pending 池：全部 queued 任务按就绪时间 FIFO（同阶段按 id——
    /// 插入序即下发序；跨构建按 build_id——构建号大序即就绪先后），
    /// spec 已组装（engine 下发前完成，见 [`Self::set_spec`]）。
    pub async fn pending_pool(&self) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE status = 'queued' AND spec_json IS NOT NULL
             ORDER BY build_id, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// 调度：任务从 queued → running、占 Agent 槽位（下发未回执前即占，
    /// 防并发重复下发；回执失败由调用侧回收）。返回 false 表示行不存在
    /// 或已非 queued（并发裁决：同时只有一次下发成功）。
    pub async fn dispatch(
        &self,
        id: i64,
        agent_id: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'running', agent_id = ?, started_at = COALESCE(started_at, ?),
                 finished_at = NULL, unknown_at = NULL, waiting_detail = NULL
             WHERE id = ? AND status = 'queued'",
        )
        .bind(agent_id)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 在途回收：running/unknown → queued（下发失败/回执拒绝/Agent 离线
    /// 重发前回池重排）。不动的行（已终态/行不存在）返回 `None`。
    pub async fn revert_to_queued(&self, id: i64) -> Result<Option<JobRow>, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'queued', agent_id = NULL, started_at = NULL,
                 unknown_at = NULL, waiting_detail = NULL
             WHERE id = ? AND status IN ('running', 'unknown')",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get(id).await
    }

    /// 任务按 Agent 置 waiting_detail（pending 池匹配无果的原因标注；
    /// 供 UI 警示态，ADR-0019「无匹配 Agent/缺标签」分类）。
    pub async fn set_waiting(&self, id: i64, detail: Option<&str>) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE jobs SET waiting_detail = ? WHERE id = ?")
            .bind(detail)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 超时扫描：返回已到点（running 且 (now - started_at)/60000 >=
    /// timeout_minutes，timeout_minutes > 0）的任务行——状态迁移由调用侧
    /// （sched）经 [`Self::transition`] 置 timeout 终态。返回行保证仍
    /// running（条件更新防重复裁决）；终态吸收使重复扫描幂等。
    pub async fn timeout_due(&self, now: i64) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE status = 'running'
               AND timeout_minutes > 0
               AND started_at IS NOT NULL
               AND (? - started_at) / 60000 >= timeout_minutes
             ORDER BY id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// orphan 宽限扫描输入：unknown 状态、按 unknown_at 排序输出稳定
    /// （调用侧裁决到点后经 [`Self::mark_orphan_failed`] 判败）。
    pub async fn unknown_jobs(&self) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE status = 'unknown' ORDER BY unknown_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// orphan 宽限超时判败（unknown → failed，detail 记宽限超时；调用侧
    /// 已按宽限分钟裁决到点）。返回 false 表示行不存在或已非 unknown。
    pub async fn mark_orphan_failed(&self, id: i64, now: i64) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'failed', finished_at = ?,
                 detail = COALESCE(detail, 'orphan 宽限超时：Agent 未恢复')
             WHERE id = ? AND status = 'unknown'",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 排队中任务移出 pending 池（构建级取消：queued → cancelled，不落
    /// 开始时刻——未运行）。返回受影响行数。
    pub async fn cancel_queued_by_build(&self, build_id: i64, now: i64) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'cancelled', finished_at = ?, detail = COALESCE(detail, 'build 取消')
             WHERE build_id = ? AND status = 'queued'",
        )
        .bind(now)
        .bind(build_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 构建在途任务清单（运行中构建取消下发 CancelBuild 用）。
    pub async fn in_flight_by_build(&self, build_id: i64) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE build_id = ? AND status IN ('running', 'unknown')
             ORDER BY id",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// Agent 在途任务清单（Agent 离线转 unknown、重连重挂、补发挂起取消的
    /// 输入面）。
    pub async fn by_agent(&self, agent_id: i64) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs WHERE agent_id = ? AND status IN ('running', 'unknown')
             ORDER BY id",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// Agent 判离线：该 Agent 的 running 任务全部转 unknown（离线不判死，
    /// 重连回归 running；unknown_at 记此刻起宽限计时，ADR-0008）。返回
    /// 受影响行数。
    pub async fn agent_offline_to_unknown(&self, agent_id: i64, now: i64) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'unknown', unknown_at = ?,
                 detail = COALESCE(detail, 'agent 离线')
             WHERE agent_id = ? AND status = 'running'",
        )
        .bind(now)
        .bind(agent_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 在途任务按 Agent 回收：running/unknown → queued（Agent 重启丢任务/
    /// 上报不一致时由调用侧裁决——恢复调度的候选，等重新匹配）。
    pub async fn in_flight_by_agent_to_queued(&self, agent_id: i64) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET status = 'queued', agent_id = NULL, started_at = NULL,
                 unknown_at = NULL, waiting_detail = NULL
             WHERE agent_id = ? AND status IN ('running', 'unknown')",
        )
        .bind(agent_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 挂起取消视图（全库）：构建已取消/失败（含 fail-fast 级联）但任务仍
    /// 在途（running/unknown、agent_id 非空）——CancelBuild 补发的输入面
    /// （离线者挂起，重连经 [`Self::channel_cancel_pending_for_agent`] 对账）。
    pub async fn channel_cancel_pending(&self) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT j.id, j.build_id, j.stage_index, j.name, j.status, j.attempt, j.spec_json,
                    j.agent_id, j.labels, j.timeout_minutes, j.retry_count, j.allow_failure,
                    j.started_at, j.finished_at, j.exit_code, j.detail, j.unknown_at, j.waiting_detail
             FROM jobs j JOIN builds b ON b.id = j.build_id
             WHERE b.status IN ('cancelled', 'failed')
               AND j.status IN ('running', 'unknown')
               AND j.agent_id IS NOT NULL
             ORDER BY j.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// 挂起取消视图（按 Agent）：同上但限定某 Agent（重连 JobReported 对账
    /// 补发输入面）。
    pub async fn channel_cancel_pending_for_agent(
        &self,
        agent_id: i64,
    ) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT j.id, j.build_id, j.stage_index, j.name, j.status, j.attempt, j.spec_json,
                    j.agent_id, j.labels, j.timeout_minutes, j.retry_count, j.allow_failure,
                    j.started_at, j.finished_at, j.exit_code, j.detail, j.unknown_at, j.waiting_detail
             FROM jobs j JOIN builds b ON b.id = j.build_id
             WHERE b.status IN ('cancelled', 'failed')
               AND j.status IN ('running', 'unknown')
               AND j.agent_id = ?
             ORDER BY j.id",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }

    /// 挂起取消视图（按构建）：同上但限定某构建（fail-fast 级联事件补发
    /// 输入面）。
    pub async fn channel_cancel_pending_for_build(
        &self,
        build_id: i64,
    ) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query_as::<_, JobRowTuple>(
            "SELECT id, build_id, stage_index, name, status, attempt, spec_json, agent_id,
                    labels, timeout_minutes, retry_count, allow_failure,
                    started_at, finished_at, exit_code, detail, unknown_at, waiting_detail
             FROM jobs
             WHERE build_id = ? AND status IN ('running', 'unknown')
               AND agent_id IS NOT NULL
             ORDER BY id",
        )
        .bind(build_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(JobRow::from_tuple).collect()
    }
}

/// jobs 行元组（列形态唯一收敛点，免逐查询散落 `Row::get`）。struct 而非
/// 元组：列数超 sqlx 的 16 列元组上限（`unknown_at`/`waiting_detail` 为
/// B2c-T4 加列后共 18 列）。
#[derive(sqlx::FromRow)]
struct JobRowTuple {
    id: i64,
    build_id: i64,
    stage_index: i32,
    name: String,
    status: String,
    attempt: i32,
    spec_json: Option<String>,
    agent_id: Option<i64>,
    labels: String,
    timeout_minutes: i32,
    retry_count: i32,
    allow_failure: bool,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    exit_code: Option<i32>,
    detail: Option<String>,
    unknown_at: Option<i64>,
    waiting_detail: Option<String>,
}

impl JobRow {
    /// 手工行映射（未知状态取值视为库损坏，与 ScmType 同纪律）。
    fn from_tuple(row: JobRowTuple) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            build_id: row.build_id,
            stage_index: row.stage_index,
            name: row.name,
            status: JobStatus::parse(&row.status)?,
            attempt: row.attempt,
            spec_json: row.spec_json,
            agent_id: row.agent_id,
            labels: row.labels,
            timeout_minutes: row.timeout_minutes,
            retry_count: row.retry_count,
            allow_failure: row.allow_failure,
            started_at: row.started_at,
            finished_at: row.finished_at,
            exit_code: row.exit_code,
            detail: row.detail,
            unknown_at: row.unknown_at,
            waiting_detail: row.waiting_detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::builds::{BuildRepo, StartBuild, TriggerSource};
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};
    use sisyphus_model::pipeline::{Job, Pipeline, Shell, Stage, Step};
    use sisyphus_model::validate::BuildSnapshot;
    use sisyphus_model::pipeline::Revision;

    /// 独立临时目录 + 已迁移库 + 预置项目 + 一个 queued 构建（store 缝形态）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool, i64) {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = super::super::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        let project = ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        let pipeline = Pipeline {
            name: "build".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec!["sisyphus/os=linux".into()],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "cargo build".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        };
        let build = BuildRepo::new(pool.clone())
            .start(StartBuild {
                project_id: project.id,
                pipeline_name: "build".into(),
                trigger: TriggerSource::Manual,
                trigger_detail: r#"{"by":"alice"}"#.into(),
                snapshot: BuildSnapshot::new(
                    pipeline,
                    Revision {
                        number: 1,
                        operator: "tester".into(),
                        at_ms: 1_000,
                    },
                ),
            })
            .await
            .expect("开始构建");
        (dir, pool, build.id)
    }

    fn new_job(build_id: i64, name: &str) -> NewJob {
        NewJob {
            build_id,
            stage_index: 0,
            name: name.into(),
            attempt: 1,
            spec_json: Some(r#"{"name":"compile"}"#.into()),
            agent_id: None,
            labels: vec!["sisyphus/os=linux".into()],
            timeout_minutes: 30,
            retry_count: 2,
            allow_failure: false,
        }
    }

    #[tokio::test]
    async fn insert_and_list_round_trip_with_labels_json() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        // 先建真实 Agent 行（agent_id 为外键，引用必须存在）。
        let agent = crate::store::agents::AgentRepo::new(pool.clone())
            .create(crate::store::agents::NewAgent {
                name: "linux-1".into(),
                token_hash: "sisa-hash-linux-1".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-linux-1".into(),
                register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
            })
            .await
            .expect("建 Agent");

        let a = repo
            .insert(new_job(build_id, "compile"))
            .await
            .expect("首任务");
        let b = repo
            .insert(NewJob {
                stage_index: 1,
                name: "lint".into(),
                agent_id: Some(agent.id),
                ..new_job(build_id, "lint")
            })
            .await
            .expect("二任务");

        assert!(a.id > 0 && b.id > 0);
        assert_eq!(a.status, JobStatus::Queued);
        assert_eq!(a.attempt, 1);
        assert_eq!(a.agent_id, None);
        assert_eq!(a.spec_json.as_deref(), Some(r#"{"name":"compile"}"#));
        assert_eq!(b.agent_id, Some(agent.id));
        assert_eq!(b.stage_index, 1);

        // labels 落库为 JSON 数组文本、读回等价。
        assert!(a.labels.contains("sisyphus/os=linux"));
        let labels: Vec<String> = serde_json::from_str(&a.labels).expect("labels 应可解析");
        assert_eq!(labels, vec!["sisyphus/os=linux"]);

        let all = repo.list_by_build(build_id).await.expect("清单");
        assert_eq!(all.len(), 2, "构建下全部任务可读回");
        let queued = repo.queued_by_build(build_id).await.expect("待跑");
        assert_eq!(queued.len(), 2, "全部 queued 在待跑集");
    }

    /// 票 #45 AC：任务状态迁移全集合可经 repo 读写——running→unknown→
    /// running（离线不判死，重连回归且首启时刻保留）、unknown→宽限超时→
    /// failed、cancel/timeout 终态。
    #[tokio::test]
    async fn full_status_set_migrates_and_offline_returns_to_running() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let job = repo.insert(new_job(build_id, "compile")).await.expect("建");

        // queued→running：记 started_at。
        assert!(repo
            .transition(job.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("进运行"));
        let running = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(running.started_at, Some(1_000));

        // running→unknown（离线不判死）：unknown 不是终态，可再迁移。
        assert!(repo
            .transition(job.id, JobStatus::Unknown, None, Some("agent offline"), 2_000)
            .await
            .expect("转 unknown"));
        let unknown = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(unknown.status, JobStatus::Unknown);
        assert_eq!(unknown.finished_at, None, "unknown 非终态");
        assert_eq!(unknown.unknown_at, Some(2_000), "unknown_at 记宽限计时起点");

        // unknown→running（重连回归）：started_at 保留首启时刻、unknown_at 清空。
        assert!(repo
            .transition(job.id, JobStatus::Running, None, None, 3_000)
            .await
            .expect("回归运行"));
        let back = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(back.status, JobStatus::Running);
        assert_eq!(back.started_at, Some(1_000), "首启时刻保留");
        assert_eq!(back.detail.as_deref(), Some("agent offline"), "历史 detail 保留");
        assert_eq!(back.unknown_at, None, "重连回归清空宽限计时");

        // running→succeeded：终态、记 finished_at、留退出码。
        assert!(repo
            .transition(job.id, JobStatus::Succeeded, Some(0), None, 4_000)
            .await
            .expect("成功"));
        let done = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(done.status, JobStatus::Succeeded);
        assert_eq!(done.finished_at, Some(4_000));
        assert_eq!(done.exit_code, Some(0));

        // 终态吸收：succeeded 不可再迁移。
        assert!(!repo
            .transition(job.id, JobStatus::Failed, None, None, 5_000)
            .await
            .expect("终态迁移应拒绝"));

        // 另一任务走 unknown→failed（宽限超时判败路径由调用侧裁决，此处
        // 验迁移本身）与 cancelled/timeout/aborted/skipped 终态。
        let other = repo.insert(new_job(build_id, "other")).await.expect("建");
        repo.transition(other.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("进运行");
        assert!(repo
            .transition(other.id, JobStatus::Unknown, None, None, 2_000)
            .await
            .expect("转 unknown"));
        assert!(repo
            .transition(other.id, JobStatus::Failed, Some(1), Some("orphan timeout"), 3_000)
            .await
            .expect("宽限超时判败"));
        assert_eq!(
            repo.get(other.id).await.expect("查").expect("应存在").status,
            JobStatus::Failed
        );

        for (name, to) in [
            ("cancelled", JobStatus::Cancelled),
            ("timeout", JobStatus::Timeout),
            ("aborted", JobStatus::Aborted),
            ("skipped", JobStatus::Skipped),
        ] {
            let job = repo.insert(new_job(build_id, name)).await.expect("建");
            assert!(
                repo.transition(job.id, to, None, None, 6_000)
                    .await
                    .expect("终态迁移"),
                "{name} 迁移应成功"
            );
            assert!(
                to.is_terminal() && !to.occupies_slot(),
                "{name} 应为终态且不占槽"
            );
            assert_eq!(
                repo.get(job.id).await.expect("查").expect("应存在").started_at,
                None,
                "排队中直接终态不得有开始时刻（started_at 只属 queued→running）"
            );
        }
    }

    /// 票 #45 AC：attempt 递增、成功任务保留——重跑同任务占新行 attempt+1，
    /// 原行（含成功结果）不动。
    #[tokio::test]
    async fn next_attempt_bumps_attempt_and_keeps_previous_row() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let job = repo.insert(new_job(build_id, "compile")).await.expect("建");

        // 运行中不可重跑（非失败类终态）。
        repo.transition(job.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("进运行");
        assert!(
            repo.next_attempt(job.id).await.expect("重跑").is_none(),
            "运行中不可开新 attempt"
        );

        // 失败后可重跑：新行 attempt+1、spec/labels/语义字段复制、agent 清空。
        repo.transition(job.id, JobStatus::Failed, Some(1), Some("boom"), 2_000)
            .await
            .expect("失败");
        let rerun = repo
            .next_attempt(job.id)
            .await
            .expect("重跑")
            .expect("失败任务应可重跑");
        assert_eq!(rerun.attempt, 2);
        assert_eq!(rerun.status, JobStatus::Queued);
        assert_eq!(rerun.agent_id, None, "新 attempt 重新调度");
        assert_eq!(rerun.spec_json, job.spec_json, "spec 快照延续");
        assert_eq!(rerun.timeout_minutes, 30);
        assert_eq!(rerun.retry_count, 2);
        assert_eq!(rerun.build_id, build_id);
        assert_ne!(rerun.id, job.id, "新行");

        // 原行保留失败结果（成功/失败历史不丢）。
        let original = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(original.status, JobStatus::Failed);
        assert_eq!(original.finished_at, Some(2_000));

        // 成功任务不重跑：再建一个成功任务，next_attempt 返回 None。
        let ok = repo.insert(new_job(build_id, "ok")).await.expect("建");
        repo.transition(ok.id, JobStatus::Succeeded, Some(0), None, 3_000)
            .await
            .expect("成功");
        assert!(
            repo.next_attempt(ok.id).await.expect("重跑").is_none(),
            "成功任务不参与重跑"
        );
    }

    #[tokio::test]
    async fn active_by_agent_counts_slot_occupancy() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        // 两个真实 Agent（agent_id 为外键，引用必须存在）。
        let agents = crate::store::agents::AgentRepo::new(pool.clone());
        let agent_a = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-1".into(),
                token_hash: "sisa-hash-linux-1".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-linux-1".into(),
                register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
            })
            .await
            .expect("linux-1");
        let agent_b = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-2".into(),
                token_hash: "sisa-hash-linux-2".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-linux-2".into(),
                register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
            })
            .await
            .expect("linux-2");

        let a = repo
            .insert(NewJob {
                agent_id: Some(agent_a.id),
                ..new_job(build_id, "a")
            })
            .await
            .expect("a");
        let b = repo
            .insert(NewJob {
                agent_id: Some(agent_a.id),
                ..new_job(build_id, "b")
            })
            .await
            .expect("b");
        repo.insert(NewJob {
            agent_id: Some(agent_b.id),
            ..new_job(build_id, "c")
        })
        .await
        .expect("c");

        assert_eq!(
            repo.active_by_agent(agent_a.id).await.expect("槽位"),
            0,
            "queued 不占槽"
        );

        // running/unknown 各占一槽；终态释放。
        repo.transition(a.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("a 运行");
        repo.transition(b.id, JobStatus::Unknown, None, None, 1_000)
            .await
            .expect("b unknown");
        assert_eq!(repo.active_by_agent(agent_a.id).await.expect("槽位"), 2);
        assert_eq!(repo.active_by_agent(agent_b.id).await.expect("槽位"), 0);

        repo.transition(a.id, JobStatus::Succeeded, Some(0), None, 2_000)
            .await
            .expect("a 完成");
        assert_eq!(
            repo.active_by_agent(agent_a.id).await.expect("槽位"),
            1,
            "终态释放槽位"
        );
    }

    #[tokio::test]
    async fn queued_set_excludes_terminal_jobs() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let done = repo.insert(new_job(build_id, "done")).await.expect("done");
        repo.insert(new_job(build_id, "pending")).await.expect("pending");

        repo.transition(done.id, JobStatus::Succeeded, Some(0), None, 1_000)
            .await
            .expect("完成");
        let queued = repo.queued_by_build(build_id).await.expect("待跑");
        let names: Vec<&str> = queued.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["pending"], "终态任务不在待跑集");
    }

    // -----------------------------------------------------------------------
    // sched 面（票 B2c-T4）：pending 池 / 调度回收 / 超时 / 宽限 / 取消 /
    // 离线转 unknown。只测外部行为与状态机结果（不测 SQL 文本）。
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pending_pool_fifo_excludes_spec_missing_and_terminal() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let a = repo.insert(new_job(build_id, "a")).await.expect("a"); // spec 有
        let _b = repo
            .insert(NewJob {
                spec_json: None, // 未组装（engine 未下发）
                ..new_job(build_id, "b")
            })
            .await
            .expect("b");
        let c = repo.insert(new_job(build_id, "c")).await.expect("c");

        repo.transition(c.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("c 运行");
        repo.transition(a.id, JobStatus::Succeeded, Some(0), None, 2_000)
            .await
            .expect("a 完成");

        let pool_rows = repo.pending_pool().await.expect("pending 池");
        assert!(pool_rows.is_empty(), "a 终态、c 在途、b 无 spec——都不在待跑集");
    }

    #[tokio::test]
    async fn dispatch_occupies_slot_and_revert_returns_to_pool() {
        let (_dir, pool, build_id) = fixture().await;
        let agents = crate::store::agents::AgentRepo::new(pool.clone());
        let agent = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-1".into(),
                token_hash: "sisa-hash-sched-1".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-sched-1".into(),
                register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
            })
            .await
            .expect("建 Agent");
        agents.mark_online(agent.id, "[]", None, 1_000).await.expect("上线");
        let repo = JobRepo::new(pool.clone());
        let job = repo.insert(new_job(build_id, "compile")).await.expect("建");

        // 下发：queued → running、占槽。
        assert!(repo.dispatch(job.id, agent.id, 1_000).await.expect("下发"));
        let running = repo.get(job.id).await.expect("查").expect("应存在");
        assert_eq!(running.status, JobStatus::Running);
        assert_eq!(running.agent_id, Some(agent.id));
        assert_eq!(running.started_at, Some(1_000), "下发记 started_at（超时计时起点）");
        assert_eq!(repo.active_by_agent(agent.id).await.expect("在途"), 1, "下发即占槽");

        // 并发裁决：非 queued 再下发返回 false（同时只有一次赢）。
        assert!(!repo.dispatch(job.id, agent.id, 1_100).await.expect("重复下发"));

        // 回收：running → queued、清 agent/started_at/unknown_at/waiting（重发前回池）。
        let reverted = repo
            .revert_to_queued(job.id)
            .await
            .expect("回收")
            .expect("应有回收行");
        assert_eq!(reverted.status, JobStatus::Queued);
        assert_eq!(reverted.agent_id, None);
        assert_eq!(reverted.started_at, None);
        assert_eq!(repo.active_by_agent(agent.id).await.expect("在途"), 0, "回收释放槽位");

        // 已终态任务回收返回 None（不破坏终态）。
        repo.transition(job.id, JobStatus::Succeeded, Some(0), None, 1_200)
            .await
            .expect("成功");
        assert!(repo.revert_to_queued(job.id).await.expect("终态回收").is_none());
        assert_eq!(
            repo.get(job.id).await.expect("查").expect("应存在").status,
            JobStatus::Succeeded,
            "终态不被回收改写"
        );
    }

    #[tokio::test]
    async fn timeout_marks_running_jobs_with_elapsed_timeout() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let slow = repo
            .insert(NewJob {
                timeout_minutes: 30,
                ..new_job(build_id, "slow")
            })
            .await
            .expect("slow");
        let infinite = repo
            .insert(NewJob {
                timeout_minutes: 0, // 0 = 无限
                ..new_job(build_id, "infinite")
            })
            .await
            .expect("infinite");
        let tiny = repo
            .insert(NewJob {
                timeout_minutes: 1,
                ..new_job(build_id, "tiny")
            })
            .await
            .expect("tiny");

        // 全部进运行（从下发计时，started_at 即时刻 1_000）。
        for j in [&slow, &infinite, &tiny] {
            repo.transition(j.id, JobStatus::Running, None, None, 1_000)
                .await
                .expect("运行");
        }
        // 未到点：30s（tiny 的 1 分钟与 slow 的 30 分钟都未到）。
        assert!(repo.timeout_due(31_000).await.expect("扫描").is_empty());
        // 到点：tiny（1 分钟）超时；slow（30 分钟）未到；infinite 永不到点。
        let due = repo.timeout_due(61_000).await.expect("扫描");
        assert_eq!(due.len(), 1, "只有 tiny 超时");
        assert_eq!(due[0].id, tiny.id);
        assert_eq!(due[0].status, JobStatus::Running, "timeout_due 只列不迁移");
        // 状态迁移到 timeout 终态（sched 调用侧动作）→ 重复扫描幂等（已终态）。
        assert!(repo
            .transition(tiny.id, JobStatus::Timeout, None, Some("job 超时"), 61_000)
            .await
            .expect("超时终态"));
        assert!(repo.timeout_due(61_000).await.expect("再扫").is_empty());
    }

    #[tokio::test]
    async fn orphan_grace_marks_unknown_jobs_failed() {
        let (_dir, pool, build_id) = fixture().await;
        let repo = JobRepo::new(pool.clone());
        let orphan = repo.insert(new_job(build_id, "orphan")).await.expect("orphan");

        repo.transition(orphan.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("运行");
        repo.transition(orphan.id, JobStatus::Unknown, None, None, 2_000)
            .await
            .expect("离线转 unknown");

        let unknowns = repo.unknown_jobs().await.expect("unknown 清单");
        assert_eq!(unknowns.len(), 1);
        assert_eq!(unknowns[0].unknown_at, Some(2_000), "unknown_at 计时起点可查");

        // 宽限超时判败（调用侧裁决到点后经此落库）。
        assert!(repo
            .mark_orphan_failed(orphan.id, 2_000 + 10 * 60_000)
            .await
            .expect("判败"));
        let failed = repo.get(orphan.id).await.expect("查").expect("应存在");
        assert_eq!(failed.status, JobStatus::Failed);
        assert!(failed.detail.as_deref().is_some(), "宽限超时 detail 记名");
        // 终态吸收：已 failed 不可再判败。
        assert!(!repo.mark_orphan_failed(orphan.id, 3_000_000).await.expect("再判败"));
    }

    #[tokio::test]
    async fn agent_offline_marks_running_unknown_and_cancel_by_build() {
        let (_dir, pool, build_id) = fixture().await;
        let agents = crate::store::agents::AgentRepo::new(pool.clone());
        let agent = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-1".into(),
                token_hash: "sisa-hash-sched-2".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 2,
                register_code_hash: "code-hash-sched-2".into(),
                register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
            })
            .await
            .expect("建 Agent");
        let repo = JobRepo::new(pool.clone());
        let running = repo.insert(new_job(build_id, "running")).await.expect("running");
        let queued = repo.insert(new_job(build_id, "queued")).await.expect("queued");
        repo.dispatch(running.id, agent.id, 1_000).await.expect("下发 running");

        // Agent 判离线：running → unknown、记 unknown_at；queued 不受影响。
        assert_eq!(
            repo.agent_offline_to_unknown(agent.id, 2_000).await.expect("转 unknown"),
            1
        );
        assert_eq!(
            repo.get(running.id).await.expect("查").expect("应存在").status,
            JobStatus::Unknown
        );
        assert_eq!(
            repo.get(queued.id).await.expect("查").expect("应存在").status,
            JobStatus::Queued,
            "排队任务不因 Agent 离线而变"
        );

        // 构建级取消：排队中移出（不落开始时刻），在途不受影响（经通道下发取消）。
        assert_eq!(repo.cancel_queued_by_build(build_id, 3_000).await.expect("取消"), 1);
        let queued_now = repo.get(queued.id).await.expect("查").expect("应存在");
        assert_eq!(queued_now.status, JobStatus::Cancelled);
        assert_eq!(queued_now.started_at, None, "排队中取消无开始时刻");
        let in_flight = repo.in_flight_by_build(build_id).await.expect("在途");
        assert_eq!(in_flight.len(), 1, "unknown 在途待通道取消");
        assert_eq!(in_flight[0].id, running.id);

        // 在途任务按 Agent 回收（重启丢任务候选）。
        assert_eq!(
            repo.in_flight_by_agent_to_queued(agent.id).await.expect("回收"),
            1
        );
        let back = repo.get(running.id).await.expect("查").expect("应存在");
        assert_eq!(back.status, JobStatus::Queued);
        assert_eq!(back.agent_id, None, "回收清 agent");
    }
}

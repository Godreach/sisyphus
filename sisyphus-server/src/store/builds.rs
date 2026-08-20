//! 构建 repo（票 B2c-T1，Spec B2c §2，ADR-0006）：调度数据底座的 builds 面。
//!
//! 构建号 per-pipeline 自增：`start` 在事务内读 `MAX(number)+1` 再 INSERT，
//! 撞 `UNIQUE(project_id, pipeline_name, number)` 或 BUSY 即折算 Conflict
//! 重试——与 pipelines 并发保存同一先例，终态号恰为 1..=N 无丢失/回退/重复。
//! 构建号与快照在入队时即定（ADR-0006：定义保存不影响已入队/运行中构建）。
//!
//! FIFO 排队不在这里推进：`start` 一律落 queued，sched 经 [`Self::running_build`]
//! 判「同 pipeline 同时只跑一条」、经 [`Self::oldest_queued`] 取最老排队者
//! 提升。状态迁移经 [`Self::transition`] 条件更新（终态吸收），从失败任务
//! 重跑经 [`Self::rerun_from_failed`]（同号 attempt+1、快照/触发上下文保留）。
//! fail-fast 级联是跨 builds/jobs 两表的事务化状态迁移（[`Self::fail_fast_cascade`]）。

use sisyphus_model::validate::BuildSnapshot;
use sqlx::SqlitePool;

use super::{StoreError, is_busy, is_unique_violation, now_ms};

/// 构建号竞争/条件更新冲突的最大重试次数（与 pipelines 保存同形）。
///
/// 16 对 ~12 并发偏紧——乐观重试遇 CI 调度抖动 + BUSY 折算曾致
/// `concurrent_starts_keep_numbers_monotonic` flake（重试耗尽 panic）；
/// 抬到 64 给 N 级并发留 ~N× 余量。重试仅在竞争时发生，零稳态成本。
const MAX_START_ATTEMPTS: usize = 64;

/// 构建状态（ADR-0006 生命周期；落库文本 `as_str()` 为契约值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStatus {
    /// 已排队（FIFO 等待调度）。
    Queued,
    /// 运行中。
    Running,
    /// 全部任务成功。
    Succeeded,
    /// 任务失败（含 fail-fast 级联）。
    Failed,
    /// 构建级取消（排队中移出 / 运行中经通道下发取消）。
    Cancelled,
    /// 任务超时走取消路径的终态。
    Timeout,
}

impl BuildStatus {
    /// 落库文本（schema 取值域）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
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
            "timeout" => Ok(Self::Timeout),
            other => Err(StoreError::Db(sqlx::Error::ColumnDecode {
                index: "builds.status".into(),
                source: format!("未知 builds.status：{other}").into(),
            })),
        }
    }

    /// 终态集合：queued/running 之外皆吸收（terminal 不可再迁移）。
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// 构建触发源（手动不建 triggers 行，三种都记在 builds.trigger）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSource {
    /// 手动触发（参数覆盖 + 可选分支/commit/revision）。
    Manual,
    /// cron 定时触发。
    Cron,
    /// poll SCM 轮询触发。
    Poll,
}

impl TriggerSource {
    /// 落库文本。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Cron => "cron",
            Self::Poll => "poll",
        }
    }

    /// 从落库文本解析（未知值视为库损坏）。
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "manual" => Ok(Self::Manual),
            "cron" => Ok(Self::Cron),
            "poll" => Ok(Self::Poll),
            other => Err(StoreError::Db(sqlx::Error::ColumnDecode {
                index: "builds.trigger".into(),
                source: format!("未知 builds.trigger：{other}").into(),
            })),
        }
    }
}

/// 构建行（`trigger_detail`/`snapshot` 为 JSON 文本——schema 不解析内部，
/// 模型形态由 engine/API 层裁定，与 pipelines.definition 同纪律）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRow {
    /// 行 id（jobs 引用的外键）。
    pub id: i64,
    /// 属主项目 id。
    pub project_id: i64,
    /// pipeline 名（冗余，快照自持）。
    pub pipeline_name: String,
    /// per-pipeline 自增构建号（从 1 起）。
    pub number: i64,
    /// 构建状态。
    pub status: BuildStatus,
    /// 触发源。
    pub trigger: TriggerSource,
    /// 触发上下文 JSON（触发人/分支/commit/revision/参数覆盖）。
    pub trigger_detail: String,
    /// 重跑次数：从失败任务重跑同号 attempt+1；从头重跑占新号。
    pub attempt: i32,
    /// BuildSnapshot JSON（整份定义 + revision；机密只存名）。
    pub snapshot: String,
    /// queued→running 时刻（Unix 毫秒）。
    pub started_at: Option<i64>,
    /// 终态时刻（Unix 毫秒）。
    pub finished_at: Option<i64>,
    /// cancelled 终态时刻（Unix 毫秒）。
    pub cancelled_at: Option<i64>,
    /// 最后变更时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 开始一次构建的输入（`trigger_detail` 由触发源各自组装 JSON）。
#[derive(Debug, Clone)]
pub struct StartBuild {
    /// 属主项目 id。
    pub project_id: i64,
    /// pipeline 名（快照内亦有，此处作编号/寻径键）。
    pub pipeline_name: String,
    /// 触发源。
    pub trigger: TriggerSource,
    /// 触发上下文 JSON 文本。
    pub trigger_detail: String,
    /// 构建快照（model 类型，repo 负责序列化落库）。
    pub snapshot: BuildSnapshot,
}

/// 构建 repo：开始（编号并发单调）/ 状态迁移 / FIFO 查询 / 重跑 / fail-fast 级联。
#[derive(Debug, Clone)]
pub struct BuildRepo {
    pool: SqlitePool,
}

impl BuildRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 开始一次构建：分配 per-pipeline 下一个构建号并落 queued 行。
    ///
    /// 并发触发同一 pipeline 时，事务内 `MAX(number)+1` + UNIQUE 约束
    /// 保证每次调用拿到互不相同的号；冲突/BUSY 折算 Conflict 重试，
    /// 终态号恰为 1..=N（无丢失、无回退、无重复）。
    pub async fn start(&self, input: StartBuild) -> Result<BuildRow, StoreError> {
        let mut last_conflict = None;
        for _ in 0..MAX_START_ATTEMPTS {
            match self.start_once(&input).await {
                Ok(row) => return Ok(row),
                Err(StoreError::Conflict(e)) => last_conflict = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::Conflict(
            last_conflict.unwrap_or_else(|| "并发构建号竞争重试耗尽".into()),
        ))
    }

    /// 单次开始尝试：事务内 `MAX(number)+1` 后 INSERT（attempt=1、queued）。
    /// 唯一冲突（两个并发触发读到同一 MAX）或 BUSY 折算 Conflict 交外层重试。
    async fn start_once(&self, input: &StartBuild) -> Result<BuildRow, StoreError> {
        let now = now_ms();
        let snapshot =
            serde_json::to_string(&input.snapshot).map_err(StoreError::DefinitionJson)?;
        let mut tx = self.pool.begin().await?;

        let max: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(number) FROM builds WHERE project_id = ? AND pipeline_name = ?",
        )
        .bind(input.project_id)
        .bind(&input.pipeline_name)
        .fetch_optional(&mut *tx)
        .await?;
        let number = max.unwrap_or(0) + 1;

        let result = sqlx::query(
            "INSERT INTO builds
                (project_id, pipeline_name, number, status, trigger, trigger_detail,
                 attempt, snapshot, updated_at)
             VALUES (?, ?, ?, 'queued', ?, ?, 1, ?, ?)",
        )
        .bind(input.project_id)
        .bind(&input.pipeline_name)
        .bind(number)
        .bind(input.trigger.as_str())
        .bind(&input.trigger_detail)
        .bind(&snapshot)
        .bind(now)
        .execute(&mut *tx)
        .await;

        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) || is_busy(&e) => {
                tx.rollback().await?;
                return Err(StoreError::Conflict("并发构建号竞争".into()));
            }
            Err(e) => return Err(e.into()),
        };

        tx.commit().await?;
        Ok(BuildRow {
            id: result.last_insert_rowid(),
            project_id: input.project_id,
            pipeline_name: input.pipeline_name.clone(),
            number,
            status: BuildStatus::Queued,
            trigger: input.trigger,
            trigger_detail: input.trigger_detail.clone(),
            attempt: 1,
            snapshot,
            started_at: None,
            finished_at: None,
            cancelled_at: None,
            updated_at: now,
        })
    }

    /// 状态迁移（条件更新，终态吸收）：queued/running → 目标状态。
    ///
    /// 只有进 running 才记 `started_at`（首次运行时刻，已有时保留）；
    /// 排队中直接取消/失败的构建不落开始时刻（Spec B2c §2：started_at
    /// 即 queued→running 时刻）。进终态记 `finished_at`；Cancelled 另记
    /// `cancelled_at`。已是终态或行不存在返回 `false`（调用侧 404 / 状态
    /// 竞态裁决）。
    pub async fn transition(&self, id: i64, to: BuildStatus, now: i64) -> Result<bool, StoreError> {
        let finished = to.is_terminal().then_some(now);
        let cancelled = (to == BuildStatus::Cancelled).then_some(now);
        let started = (to == BuildStatus::Running).then_some(now);
        // RETURNING started_at（COALESCE 后的实际值）：进终态时算时长
        // 直方图（ADR-0019，票 B5-T7）。终态计数/时长在「条件更新真正命中」
        // 时记——同一行只有一次迁移能命中（终态吸收），天然单次，事件广播
        // 面（engine publish_build_status）的重复发布不会重复计数。
        let row: Option<(i64, Option<i64>)> = sqlx::query_as(
            "UPDATE builds
             SET status = ?, started_at = COALESCE(started_at, ?),
                 finished_at = ?, cancelled_at = ?, updated_at = ?
             WHERE id = ? AND status IN ('queued', 'running')
             RETURNING id, started_at",
        )
        .bind(to.as_str())
        .bind(started)
        .bind(finished)
        .bind(cancelled)
        .bind(now)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((_, started_at)) => {
                if to.is_terminal() {
                    // 无 started_at（排队中直接取消）不记时长——record 侧
                    // 已判 duration_ms > 0。
                    let duration_ms = started_at.map(|s| now - s).unwrap_or(0);
                    crate::metrics::record_build_terminal(to.as_str(), duration_ms);
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// FIFO 放行（ADR-0006：同 pipeline 同时只跑一条，后来者排队）：
    /// 单条 UPDATE 内「无运行中构建且存在排队者」→ 提升号最小的排队者
    /// 进 running（记 started_at、清终态时间戳）。原子裁决：并发调度循环
    /// 各自调用，恰一个赢家（写事务串行 + NOT EXISTS 读已提交态），其余
    /// 返回 `None`。engine 的 drive 与 sched 循环共用此缝。
    pub async fn promote_oldest_if_idle(
        &self,
        project_id: i64,
        pipeline_name: &str,
        now: i64,
    ) -> Result<Option<BuildRow>, StoreError> {
        let row = sqlx::query_as::<_, BuildTuple>(
            "UPDATE builds
             SET status = 'running', started_at = COALESCE(started_at, ?),
                 finished_at = NULL, cancelled_at = NULL, updated_at = ?
             WHERE id = (
                 SELECT id FROM builds
                 WHERE project_id = ? AND pipeline_name = ? AND status = 'queued'
                 ORDER BY number LIMIT 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM builds
                 WHERE project_id = ? AND pipeline_name = ? AND status = 'running'
             )
             RETURNING id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                       attempt, snapshot, started_at, finished_at, cancelled_at, updated_at",
        )
        .bind(now)
        .bind(now)
        .bind(project_id)
        .bind(pipeline_name)
        .bind(project_id)
        .bind(pipeline_name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(BuildRow::from_tuple).transpose()
    }

    /// 同 pipeline 正在运行的构建（FIFO 判「同时只跑一条」；没有返回 `None`）。
    pub async fn running_build(
        &self,
        project_id: i64,
        pipeline_name: &str,
    ) -> Result<Option<BuildRow>, StoreError> {
        let row = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds
             WHERE project_id = ? AND pipeline_name = ? AND status = 'running'
             ORDER BY number LIMIT 1",
        )
        .bind(project_id)
        .bind(pipeline_name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(BuildRow::from_tuple).transpose()
    }

    /// 同 pipeline 号最小的排队构建（FIFO 队列头；没有返回 `None`）。
    pub async fn oldest_queued(
        &self,
        project_id: i64,
        pipeline_name: &str,
    ) -> Result<Option<BuildRow>, StoreError> {
        let row = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds
             WHERE project_id = ? AND pipeline_name = ? AND status = 'queued'
             ORDER BY number LIMIT 1",
        )
        .bind(project_id)
        .bind(pipeline_name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(BuildRow::from_tuple).transpose()
    }

    /// 从失败任务重跑（同号延续，ADR-0006 / Spec B2c §7 `mode=from_failed`）：
    /// attempt+1、状态回 queued、清终态时间戳；快照与触发上下文保留（重跑
    /// 仍在「构建 #N 当时」语义下）。可重跑的终态为 failed/cancelled/timeout
    /// ——取消/超时构建同样有已成功任务值得按同号续跑；succeeded 与运行中
    /// 不可重跑。构建不存在或不可重跑返回 `None`。
    pub async fn rerun_from_failed(&self, id: i64) -> Result<Option<BuildRow>, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE builds
             SET attempt = attempt + 1, status = 'queued',
                 started_at = NULL, finished_at = NULL, cancelled_at = NULL,
                 updated_at = ?
             WHERE id = ? AND status IN ('failed', 'cancelled', 'timeout')",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get(id).await
    }

    /// fail-fast 级联（ADR-0006）：一事务内——同阶段非终态任务 → cancelled、
    /// 后续阶段 queued 任务 → skipped、构建 → failed。running/unknown 的
    /// 同阶段任务也直接置 cancelled（挂起的通道取消随 grpc 批次补发）。
    pub async fn fail_fast_cascade(
        &self,
        build_id: i64,
        failed_stage_index: i32,
        now: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE jobs
             SET status = 'cancelled', finished_at = ?,
                 unknown_at = NULL, waiting_detail = NULL
             WHERE build_id = ? AND stage_index = ?
               AND status IN ('queued', 'running', 'unknown')",
        )
        .bind(now)
        .bind(build_id)
        .bind(failed_stage_index)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE jobs
             SET status = 'skipped', finished_at = ?,
                 unknown_at = NULL, waiting_detail = NULL
             WHERE build_id = ? AND stage_index > ?
               AND status IN ('queued', 'running', 'unknown')",
        )
        .bind(now)
        .bind(build_id)
        .bind(failed_stage_index)
        .execute(&mut *tx)
        .await?;
        // 构建 → failed：条件更新唯一命中一次（终态吸收），在此记终态
        // 指标（ADR-0019，票 B5-T7）。与 [`Self::transition`] 一致，避免
        // engine 事件广播面对已终态行的重复发布造成重复计数。
        let row: Option<(i64, Option<i64>)> = sqlx::query_as(
            "UPDATE builds SET status = 'failed', finished_at = ?, updated_at = ?
             WHERE id = ? AND status IN ('queued', 'running')
             RETURNING id, started_at",
        )
        .bind(now)
        .bind(now)
        .bind(build_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((_, started_at)) = row {
            let duration_ms = started_at.map(|s| now - s).unwrap_or(0);
            crate::metrics::record_build_terminal("failed", duration_ms);
        }
        tx.commit().await?;
        Ok(())
    }

    /// 按行 id 取构建；不存在返回 `None`。
    pub async fn get(&self, id: i64) -> Result<Option<BuildRow>, StoreError> {
        let row = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(BuildRow::from_tuple).transpose()
    }

    /// 全部非终态构建（queued/running——启动重建的 drive 输入面，ADR-0008：
    /// 重启从库重建调度状态）。
    pub async fn non_terminal(&self) -> Result<Vec<BuildRow>, StoreError> {
        let rows = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds WHERE status IN ('queued', 'running') ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(BuildRow::from_tuple).collect()
    }

    /// 按 (project, pipeline, number) 取构建；不存在返回 `None`（REST 详情
    /// 寻径——构建号 per-pipeline，须带 pipeline_name 唯一定位）。
    pub async fn get_by_number(
        &self,
        project_id: i64,
        pipeline_name: &str,
        number: i64,
    ) -> Result<Option<BuildRow>, StoreError> {
        let row = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds WHERE project_id = ? AND pipeline_name = ? AND number = ?",
        )
        .bind(project_id)
        .bind(pipeline_name)
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        row.map(BuildRow::from_tuple).transpose()
    }

    /// 项目的构建列表（按号倒序，REST 列表面形态）。
    pub async fn list_by_project(&self, project_id: i64) -> Result<Vec<BuildRow>, StoreError> {
        let rows = sqlx::query_as::<_, BuildTuple>(
            "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                    attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
             FROM builds WHERE project_id = ? ORDER BY number DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(BuildRow::from_tuple).collect()
    }

    /// 项目的某 pipeline 构建列表分页（按号倒序，REST 列表面 + 状态过滤，
    /// 票 B2c-T5）。`status` 为 `None` 时不过滤；`limit`/`offset` 由调用侧校验
    /// （page/limit 归一化）。运行时查询（非 `query!` 宏，不动 `.sqlx`）；
    /// `status` 取自枚举 `as_str()`，无注入面。
    pub async fn list_page(
        &self,
        project_id: i64,
        pipeline_name: &str,
        status: Option<BuildStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<BuildRow>, StoreError> {
        let rows =
            match status {
                Some(status) => sqlx::query_as::<_, BuildTuple>(
                    "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                        attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
                 FROM builds
                 WHERE project_id = ? AND pipeline_name = ? AND status = ?
                 ORDER BY number DESC LIMIT ? OFFSET ?",
                )
                .bind(project_id)
                .bind(pipeline_name)
                .bind(status.as_str())
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?,
                None => sqlx::query_as::<_, BuildTuple>(
                    "SELECT id, project_id, pipeline_name, number, status, trigger, trigger_detail,
                        attempt, snapshot, started_at, finished_at, cancelled_at, updated_at
                 FROM builds
                 WHERE project_id = ? AND pipeline_name = ?
                 ORDER BY number DESC LIMIT ? OFFSET ?",
                )
                .bind(project_id)
                .bind(pipeline_name)
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?,
            };
        rows.into_iter().map(BuildRow::from_tuple).collect()
    }

    /// 项目某 pipeline 构建总数（可选状态过滤；REST 列表分页的 total，
    /// 票 B2c-T5）。
    pub async fn count_by_project(
        &self,
        project_id: i64,
        pipeline_name: &str,
        status: Option<BuildStatus>,
    ) -> Result<i64, StoreError> {
        let count: i64 = match status {
            Some(status) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM builds
                 WHERE project_id = ? AND pipeline_name = ? AND status = ?",
                )
                .bind(project_id)
                .bind(pipeline_name)
                .bind(status.as_str())
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM builds WHERE project_id = ? AND pipeline_name = ?",
                )
                .bind(project_id)
                .bind(pipeline_name)
                .fetch_one(&self.pool)
                .await?
            }
        };
        Ok(count)
    }
}

/// builds 行元组（列形态唯一收敛点，免逐查询散落 `Row::get`）。
type BuildTuple = (
    i64,         // id
    i64,         // project_id
    String,      // pipeline_name
    i64,         // number
    String,      // status
    String,      // trigger
    String,      // trigger_detail
    i32,         // attempt
    String,      // snapshot
    Option<i64>, // started_at
    Option<i64>, // finished_at
    Option<i64>, // cancelled_at
    i64,         // updated_at
);

impl BuildRow {
    /// 手工行映射（未知状态取值视为库损坏，与 ScmType 同纪律）。
    fn from_tuple(row: BuildTuple) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            project_id: row.1,
            pipeline_name: row.2,
            number: row.3,
            status: BuildStatus::parse(&row.4)?,
            trigger: TriggerSource::parse(&row.5)?,
            trigger_detail: row.6,
            attempt: row.7,
            snapshot: row.8,
            started_at: row.9,
            finished_at: row.10,
            cancelled_at: row.11,
            updated_at: row.12,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_model::pipeline::Revision;
    use sisyphus_model::pipeline::{
        EnvVar, ExecutionEnv, Job, Parameter, ParameterType, ParameterValue, Pipeline, Shell,
        Stage, Step,
    };

    use crate::store::jobs::{JobRepo, JobStatus};
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};

    /// 独立临时目录 + 已迁移库 + 预置项目 demo（store 缝测试形态）。
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
        (dir, pool, project.id)
    }

    /// 全特性定义（覆盖 tagged 枚举/env/产物/机密等字段形态；机密只声明名）。
    fn full_pipeline() -> Pipeline {
        Pipeline {
            name: "release".into(),
            parameters: vec![Parameter {
                name: "target".into(),
                r#type: ParameterType::Enum,
                required: true,
                default: Some(ParameterValue::String("x86_64".into())),
                description: Some("构建目标".into()),
                choices: vec!["x86_64".into(), "aarch64".into()],
            }],
            env: vec![EnvVar {
                name: "CARGO_HOME".into(),
                value: "${SISY_WORKSPACE}/.cargo".into(),
            }],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: Some("${SISY_BRANCH} == \"main\"".into()),
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: Some(ExecutionEnv::Container {
                        image: "rust:1.97".into(),
                    }),
                    labels: vec!["sisyphus/os=linux".into()],
                    when: None,
                    env: vec![EnvVar {
                        name: "Extra".into(),
                        value: "1".into(),
                    }],
                    allow_failure: false,
                    retry_count: 2,
                    timeout_minutes: 30,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec!["DEPLOY_KEY".into()],
                    steps: vec![Step::Shell {
                        command: "cargo build --release".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        }
    }

    fn snapshot(pipeline: Pipeline) -> BuildSnapshot {
        BuildSnapshot::new(
            pipeline,
            Revision {
                number: 7,
                operator: "tester".into(),
                at_ms: 1_000_000,
            },
        )
    }

    fn start_build(project_id: i64, trigger: TriggerSource) -> StartBuild {
        StartBuild {
            project_id,
            pipeline_name: "release".into(),
            trigger,
            trigger_detail: r#"{"by":"alice","branch":"main"}"#.into(),
            snapshot: snapshot(full_pipeline()),
        }
    }

    #[tokio::test]
    async fn start_allocates_monotonic_numbers_per_pipeline() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());

        let a = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("首个");
        let b = repo
            .start(start_build(project_id, TriggerSource::Cron))
            .await
            .expect("第二个");
        let c = repo
            .start(start_build(project_id, TriggerSource::Poll))
            .await
            .expect("第三个");
        assert_eq!(
            (a.number, b.number, c.number),
            (1, 2, 3),
            "同 pipeline 构建号从 1 递增"
        );
        assert_eq!(
            (a.status, b.status, c.status),
            (
                BuildStatus::Queued,
                BuildStatus::Queued,
                BuildStatus::Queued,
            )
        );
        assert_eq!(a.attempt, 1);
        assert_eq!(a.trigger, TriggerSource::Manual);
        assert_eq!(b.trigger, TriggerSource::Cron);
        assert_eq!(c.trigger, TriggerSource::Poll);

        // 另一 pipeline 从 1 重新开始（per-pipeline 编号互不串）。
        let other = repo
            .start(StartBuild {
                pipeline_name: "lint".into(),
                ..start_build(project_id, TriggerSource::Manual)
            })
            .await
            .expect("他 pipeline");
        assert_eq!(other.number, 1);
    }

    /// 票 #45 AC：多线程并发触发同一 pipeline，UNIQUE 约束保证终态号
    /// 1..=N 无丢失/回退/重复（复用 pipelines 并发保存先例）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_starts_keep_numbers_monotonic() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        const N: usize = 12;

        let mut handles = Vec::new();
        for _ in 0..N {
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                repo.start(start_build(project_id, TriggerSource::Manual))
                    .await
                    .expect("并发开始应成功")
            }));
        }
        let mut numbers: Vec<i64> = Vec::new();
        for h in handles {
            numbers.push(h.await.expect("join").number);
        }

        numbers.sort_unstable();
        let expect: Vec<i64> = (1..=N as i64).collect();
        assert_eq!(numbers, expect, "并发开始的构建号应恰好 1..={N}");
        let max: i64 = sqlx::query_scalar(
            "SELECT MAX(number) FROM builds WHERE project_id = ? AND pipeline_name = ?",
        )
        .bind(project_id)
        .bind("release")
        .fetch_one(&pool)
        .await
        .expect("直查");
        assert_eq!(max, N as i64, "终态最大号应等于开始次数");
    }

    /// 票 #45 AC：快照落库读回与 model 类型等价；BuildSnapshot 含所用
    /// revision；机密只存名、值永不落快照。
    #[tokio::test]
    async fn snapshot_round_trips_model_equivalent_and_holds_no_secret_value() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let pipeline = full_pipeline();
        let submitted = snapshot(pipeline.clone());

        let row = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("开始");
        let stored: BuildSnapshot =
            serde_json::from_str(&row.snapshot).expect("快照应可解析为 BuildSnapshot");
        assert_eq!(stored, submitted, "快照落库读回与 model 类型等价");
        assert_eq!(stored.revision.number, 7, "快照含所用 revision");
        assert_eq!(stored.pipeline, pipeline);

        // 机密纪律：落库 JSON 里只有任务声明的机密名，值形态不存在。
        assert!(
            row.snapshot.contains("DEPLOY_KEY"),
            "快照应保留任务声明的机密名"
        );
        assert!(
            !row.snapshot.contains("super-secret-value"),
            "机密值不得出现在快照 JSON"
        );
    }

    /// 票 #45 AC：FIFO 排队——同 pipeline 同时只跑一条，后来者 queued。
    /// 本缝只立排队形状（推进由 sched 做）：号最小者最先被 oldest_queued 取到。
    #[tokio::test]
    async fn fifo_queue_second_build_is_queued() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());

        let first = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("第一条");
        let second = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("第二条");

        assert_eq!(first.number, 1);
        assert_eq!(second.number, 2);
        assert_eq!(second.status, BuildStatus::Queued, "后来者 queued");

        // 无 running：oldest_queued 取号最小者（队列头）。
        assert!(
            repo.running_build(project_id, "release")
                .await
                .expect("查")
                .is_none(),
            "无运行中构建"
        );
        let head = repo
            .oldest_queued(project_id, "release")
            .await
            .expect("查")
            .expect("应有队头");
        assert_eq!(head.id, first.id);

        // 第一条提升 running 后：running_build 命中，队头变为第二条。
        assert!(
            repo.transition(first.id, BuildStatus::Running, 1_100)
                .await
                .expect("提升")
        );
        let running = repo
            .running_build(project_id, "release")
            .await
            .expect("查")
            .expect("应有运行中");
        assert_eq!(running.id, first.id);
        assert_eq!(running.started_at, Some(1_100));
        let head = repo
            .oldest_queued(project_id, "release")
            .await
            .expect("查")
            .expect("应有队头");
        assert_eq!(head.id, second.id, "FIFO：第二条接续");
    }

    /// 票 #46：FIFO 放行原子裁决——无运行中构建时提升号最小排队者并记
    /// started_at；已有运行中构建时不动（返回 None）；运行中构建终态后
    /// 下一排队者接力（串行队列不堵死）。
    #[tokio::test]
    async fn promote_oldest_if_idle_promotes_fifo_and_is_idle_gated() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let first = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("第一条");
        let second = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("第二条");
        let third = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("第三条");

        // 无运行中：提升最老排队者（first），记 started_at、终态时间戳为空。
        let promoted = repo
            .promote_oldest_if_idle(project_id, "release", 1_000)
            .await
            .expect("放行")
            .expect("应有提升者");
        assert_eq!(promoted.id, first.id, "FIFO：号最小者先跑");
        assert_eq!(promoted.status, BuildStatus::Running);
        assert_eq!(promoted.started_at, Some(1_000));
        assert_eq!(promoted.finished_at, None);

        // 已有运行中：不动（second 仍 queued，返回 None）。
        assert!(
            repo.promote_oldest_if_idle(project_id, "release", 2_000)
                .await
                .expect("再放行")
                .is_none(),
            "运行中不得再放行"
        );
        assert_eq!(
            repo.get(second.id)
                .await
                .expect("查")
                .expect("应存在")
                .status,
            BuildStatus::Queued
        );

        // 运行中终态后：下一排队者接力（FIFO 串行队列）。
        repo.transition(first.id, BuildStatus::Succeeded, 3_000)
            .await
            .expect("first 完成");
        let next = repo
            .promote_oldest_if_idle(project_id, "release", 4_000)
            .await
            .expect("接力")
            .expect("应有提升者");
        assert_eq!(next.id, second.id, "FIFO：second 接续");
        assert_eq!(next.started_at, Some(4_000));
        assert!(
            repo.promote_oldest_if_idle(project_id, "release", 5_000)
                .await
                .expect("再放行")
                .is_none(),
            "second 运行中，third 继续排队"
        );
        assert_eq!(
            repo.get(third.id)
                .await
                .expect("查")
                .expect("应存在")
                .status,
            BuildStatus::Queued,
            "third 仍在排队"
        );
    }

    #[tokio::test]
    async fn transitions_set_timestamps_and_terminal_is_absorbing() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let row = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("开始");

        // queued→running：记 started_at；终态前可再迁移。
        assert!(
            repo.transition(row.id, BuildStatus::Running, 1_000)
                .await
                .expect("进运行")
        );
        let running = repo.get(row.id).await.expect("查").expect("应存在");
        assert_eq!(running.started_at, Some(1_000));
        assert_eq!(running.finished_at, None);

        // running→succeeded：记 finished_at、不记 cancelled_at。
        assert!(
            repo.transition(row.id, BuildStatus::Succeeded, 2_000)
                .await
                .expect("终态")
        );
        let done = repo.get(row.id).await.expect("查").expect("应存在");
        assert_eq!(done.status, BuildStatus::Succeeded);
        assert_eq!(done.finished_at, Some(2_000));
        assert_eq!(done.cancelled_at, None);
        assert_eq!(done.started_at, Some(1_000), "首启时刻保留");

        // 终态吸收：已 succeeded 不可再迁移（返回 false、状态不变）。
        assert!(
            !repo
                .transition(row.id, BuildStatus::Failed, 3_000)
                .await
                .expect("终态迁移应拒绝")
        );
        assert_eq!(
            repo.get(row.id).await.expect("查").expect("应存在").status,
            BuildStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn cancelled_sets_cancelled_at() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let row = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("开始");

        assert!(
            repo.transition(row.id, BuildStatus::Running, 1_000)
                .await
                .expect("进运行")
        );
        assert!(
            repo.transition(row.id, BuildStatus::Cancelled, 2_000)
                .await
                .expect("取消")
        );
        let done = repo.get(row.id).await.expect("查").expect("应存在");
        assert_eq!(done.status, BuildStatus::Cancelled);
        assert_eq!(done.cancelled_at, Some(2_000));
        assert_eq!(done.finished_at, Some(2_000));

        // 排队中直接取消（Spec B2c §7 排队中移出）：无 started_at——
        // 开始时刻只属 queued→running。
        let queued = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("排队构建");
        assert!(
            repo.transition(queued.id, BuildStatus::Cancelled, 3_000)
                .await
                .expect("排队中取消")
        );
        let done = repo.get(queued.id).await.expect("查").expect("应存在");
        assert_eq!(done.cancelled_at, Some(3_000));
        assert_eq!(done.started_at, None, "从未运行不得有开始时刻");
    }

    /// 票 #45 AC：从失败任务重跑——同号 attempt+1、状态回 queued、清终态
    /// 时间戳；快照保留（重跑仍在原快照语义下）。
    #[tokio::test]
    async fn rerun_from_failed_bumps_attempt_and_resets_timestamps() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let row = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("开始");
        assert!(
            repo.transition(row.id, BuildStatus::Running, 1_000)
                .await
                .expect("进运行")
        );
        assert!(
            repo.transition(row.id, BuildStatus::Failed, 2_000)
                .await
                .expect("失败")
        );
        let snapshot_before = row.snapshot.clone();

        let rerun = repo
            .rerun_from_failed(row.id)
            .await
            .expect("重跑")
            .expect("终态构建应可重跑");
        assert_eq!(rerun.number, row.number, "同号延续");
        assert_eq!(rerun.attempt, 2, "attempt+1");
        assert_eq!(rerun.status, BuildStatus::Queued);
        assert_eq!(rerun.started_at, None);
        assert_eq!(rerun.finished_at, None);
        assert_eq!(rerun.cancelled_at, None);
        assert_eq!(rerun.snapshot, snapshot_before, "快照保留");

        // 非终态构建不可重跑（running 中重复调用返回 None）。
        let running = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("新构建");
        assert!(
            repo.transition(running.id, BuildStatus::Running, 3_000)
                .await
                .expect("进运行")
        );
        assert!(
            repo.rerun_from_failed(running.id)
                .await
                .expect("重跑")
                .is_none(),
            "运行中不可重跑"
        );
    }

    /// 票 #45 AC：fail-fast 级联——同阶段非终态任务 cancelled、后续阶段
    /// queued 任务 skipped、构建 failed，全部落在一个事务里。
    #[tokio::test]
    async fn fail_fast_cascade_cancels_same_stage_and_skips_later() {
        let (_dir, pool, project_id) = fixture().await;
        let builds = BuildRepo::new(pool.clone());
        let jobs = JobRepo::new(pool.clone());
        let row = builds
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("开始");

        // 阶段 0：j0a 已成功、j0b 运行中；阶段 1：j1 排队。
        let j0a = jobs
            .insert(crate::store::jobs::NewJob {
                build_id: row.id,
                stage_index: 0,
                name: "j0a".into(),
                attempt: 1,
                spec_json: None,
                agent_id: None,
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j0a");
        let j0b = jobs
            .insert(crate::store::jobs::NewJob {
                build_id: row.id,
                stage_index: 0,
                name: "j0b".into(),
                attempt: 1,
                spec_json: None,
                agent_id: None,
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j0b");
        let _j1 = jobs
            .insert(crate::store::jobs::NewJob {
                build_id: row.id,
                stage_index: 1,
                name: "j1".into(),
                attempt: 1,
                spec_json: None,
                agent_id: None,
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j1");
        jobs.transition(j0a.id, JobStatus::Succeeded, None, None, 1_000)
            .await
            .expect("j0a 成功");
        jobs.transition(j0b.id, JobStatus::Running, None, None, 1_000)
            .await
            .expect("j0b 运行");
        assert!(
            builds
                .transition(row.id, BuildStatus::Running, 1_000)
                .await
                .expect("构建运行")
        );

        // j0b 失败 → 级联：j0b 所在阶段未完成任务 cancelled、后续阶段
        // skipped、构建 failed。
        jobs.transition(j0b.id, JobStatus::Failed, Some(1), Some("boom"), 1_200)
            .await
            .expect("j0b 失败");
        builds
            .fail_fast_cascade(row.id, 0, 1_300)
            .await
            .expect("级联");

        let after = jobs.list_by_build(row.id).await.expect("任务清单");
        let by_name: std::collections::HashMap<&str, JobStatus> =
            after.iter().map(|j| (j.name.as_str(), j.status)).collect();
        assert_eq!(by_name["j0a"], JobStatus::Succeeded, "已成功任务保留");
        assert_eq!(by_name["j0b"], JobStatus::Failed, "失败任务记失败");
        assert_eq!(by_name["j1"], JobStatus::Skipped, "后续阶段跳过");

        let build = builds.get(row.id).await.expect("查").expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed);
        assert!(build.finished_at.is_some());
    }

    #[tokio::test]
    async fn list_by_project_is_number_desc() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        for _ in 0..3 {
            repo.start(start_build(project_id, TriggerSource::Manual))
                .await
                .expect("开始");
        }
        let list = repo.list_by_project(project_id).await.expect("清单");
        let numbers: Vec<i64> = list.iter().map(|b| b.number).collect();
        assert_eq!(numbers, vec![3, 2, 1], "按号倒序");
    }

    /// 票 B2c-T5：分页 + 状态过滤 + 计数（REST 列表面）。
    #[tokio::test]
    async fn list_page_counts_and_filters_by_status() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = BuildRepo::new(pool.clone());
        let b1 = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("1");
        let b2 = repo
            .start(start_build(project_id, TriggerSource::Cron))
            .await
            .expect("2");
        let _b3 = repo
            .start(start_build(project_id, TriggerSource::Manual))
            .await
            .expect("3");
        // 1 → succeeded、2 → failed、3 留 queued（倒序 3,2,1）。
        repo.transition(b1.id, BuildStatus::Running, 1_000)
            .await
            .expect("1 运行");
        repo.transition(b1.id, BuildStatus::Succeeded, 2_000)
            .await
            .expect("1 成功");
        repo.transition(b2.id, BuildStatus::Running, 3_000)
            .await
            .expect("2 运行");
        repo.transition(b2.id, BuildStatus::Failed, 4_000)
            .await
            .expect("2 失败");

        // 倒序分页：limit 2 offset 0 → [3,2]；offset 2 → [1]。
        let page = repo
            .list_page(project_id, "release", None, 2, 0)
            .await
            .expect("页");
        assert_eq!(
            page.iter().map(|b| b.number).collect::<Vec<_>>(),
            vec![3, 2]
        );
        let page = repo
            .list_page(project_id, "release", None, 2, 2)
            .await
            .expect("页");
        assert_eq!(page.iter().map(|b| b.number).collect::<Vec<_>>(), vec![1]);

        // 总数与状态过滤计数。
        assert_eq!(
            repo.count_by_project(project_id, "release", None)
                .await
                .expect("total"),
            3
        );
        assert_eq!(
            repo.count_by_project(project_id, "release", Some(BuildStatus::Failed))
                .await
                .expect("failed total"),
            1
        );
        assert_eq!(
            repo.count_by_project(project_id, "release", Some(BuildStatus::Queued))
                .await
                .expect("queued total"),
            1
        );

        // 状态过滤分页：failed → [2]。
        let page = repo
            .list_page(project_id, "release", Some(BuildStatus::Failed), 10, 0)
            .await
            .expect("页");
        assert_eq!(page.iter().map(|b| b.number).collect::<Vec<_>>(), vec![2]);
        // 无命中状态 → 空。
        assert!(
            repo.list_page(project_id, "release", Some(BuildStatus::Cancelled), 10, 0)
                .await
                .expect("页")
                .is_empty()
        );

        // 跨 pipeline 隔离：另一 pipeline 的构建不计入本 pipeline 列表/计数。
        let other = repo
            .start(StartBuild {
                pipeline_name: "lint".into(),
                ..start_build(project_id, TriggerSource::Manual)
            })
            .await
            .expect("他 pipeline");
        assert_eq!(
            repo.count_by_project(project_id, "release", None)
                .await
                .expect("release total"),
            3,
            "他 pipeline 不计入"
        );
        assert_eq!(
            repo.count_by_project(project_id, "lint", None)
                .await
                .expect("lint total"),
            1
        );
        let _ = other;
    }
}

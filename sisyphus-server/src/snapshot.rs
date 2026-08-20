//! 概览快照（ADR-0019，票 B5-T7）：当前值聚合 + 事实型警示态，双消费——
//! REST 概览端点（stat 卡 + 警示态 + 最近构建）与 `/metrics`（经
//! [`crate::metrics::report_snapshot`] 灌同一份数）。
//!
//! 设计约束（ADR-0019）：
//! - **只展示当前值**（stat 卡，普通轮询），不做内存时序与历史曲线；
//! - **事实型警示态**（零阈值配置）：存在无匹配 Agent 的任务 / 有 Agent
//!   离线 / 存在排空·不兼容 Agent——全部由 DB 行推导，不猜阈值；
//! - 数据全从 SQLite 聚合（真相源），不遍历磁盘（产物字节 = `SUM(size)`
//!   元数据；日志字节 = `SUM(LENGTH(data))` 压缩体——与保留清理的
//!   per-build 语义同源）。
//!
//! 队列深度按等待原因分类（ADR-0019「含无匹配 Agent/缺标签原因分类」）：
//! 依据 `jobs.waiting_detail` 的文案前缀归类——文案由 [`crate::sched`] 的
//! `waiting_reason` 写入，本模块以稳定前缀映射到固定原因标签（低基数，
//! `/metrics` 的 `reason` 标签与快照的队列原因分类共用）。

use std::collections::BTreeMap;

use sqlx::SqlitePool;

use crate::store::builds::{BuildRepo, BuildRow};
use crate::store::projects::Project;
use crate::store::StoreError;

/// 等待原因前缀（与 [`crate::sched`] 的 `waiting_reason` 文案对应；解析
/// 前缀做归类，不依赖完整文案——缺标签详情可变）。
const WAIT_NO_ONLINE: &str = "等待匹配 agent：无在线 agent";
const WAIT_MISSING_TAG: &str = "等待匹配 agent：缺标签";
const WAIT_NO_SLOT: &str = "等待匹配 agent：在线 agent 无空槽";
/// 未标注原因（queued 但无 waiting_detail——尚未被匹配扫描标注）。
const WAIT_UNCATEGORIZED: &str = "uncategorized";

/// 最近构建条数上限（概览页「最近构建」区）。
pub const RECENT_BUILDS_LIMIT: usize = 10;

/// 一次快照的当前值（stat 卡 + 警示态输入的聚合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 队列深度按原因分类（BTreeMap 保序，输出稳定；键为固定原因标签）。
    pub queue: BTreeMap<&'static str, u64>,
    /// Agent 在线数。
    pub agents_online: u64,
    /// Agent 总数（含停用）。
    pub agents_total: u64,
    /// 槽位占用（running/unknown 在途任务总数）。
    pub slots_used: u64,
    /// 槽位总量（全部在线 Agent 的 max_concurrency 之和）。
    pub slots_total: u64,
    /// 构建终态计数（success/failed/cancelled/timeout）。
    pub builds_terminal: BTreeMap<String, u64>,
    /// 产物字节占用（`artifacts.size` 求和）。
    pub artifact_bytes: u64,
    /// 日志字节占用（`logs.data` 压缩体求和）。
    pub log_bytes: u64,
    /// 存在无匹配 Agent 的任务（队列中有等待原因任务）。
    pub has_no_match: bool,
    /// 存在离线 Agent（启用但离线——停用 Agent 离线是预期态，不计）。
    pub has_offline_agent: bool,
    /// 存在排空或版本不兼容 Agent（在线但不可派发）。
    pub has_draining_incompatible: bool,
}

impl Snapshot {
    /// 队列总深度（stat 卡）。
    pub fn queue_depth(&self) -> u64 {
        self.queue.values().sum()
    }
}

/// 概览快照的 REST 形态在 api/overview.rs 组装（`ToSchema` 契约面）；本模块
/// 只出 [`Snapshot`] 真值 + [`recent_builds`]，rest 形态由 api 层合流。
///
/// 最近构建条目（概览页列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentBuild {
    /// 项目名。
    pub project: String,
    /// pipeline 名。
    pub pipeline: String,
    /// per-pipeline 构建号。
    pub number: i64,
    /// 状态（REST 同值域）。
    pub status: &'static str,
    /// 触发源（manual/cron/poll）。
    pub trigger: &'static str,
    /// 开始时刻（Unix 毫秒；未运行 null）。
    pub started_at: Option<i64>,
    /// 终态时刻（Unix 毫秒）。
    pub finished_at: Option<i64>,
}

impl RecentBuild {
    fn from_row(project_name: &str, row: &BuildRow) -> Self {
        Self {
            project: project_name.to_string(),
            pipeline: row.pipeline_name.clone(),
            number: row.number,
            status: row.status.as_str(),
            trigger: row.trigger.as_str(),
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}

/// 聚合全部当前值（DB 真相源）。任意聚合失败即整体失败（快照端点 500——
/// 概览是低负载轮询面，不做部分成功；DB 故障时 UI 已有 loadError 降级面）。
pub async fn compute(pool: &SqlitePool) -> Result<Snapshot, StoreError> {
    let queue = queue_by_reason(pool).await?;
    let agents = agent_stats(pool).await?;
    let slots = slot_stats(pool).await?;
    let builds_terminal = terminal_counts(pool).await?;
    let artifact_bytes = scalar_sum(pool, "SELECT COALESCE(SUM(size), 0) FROM artifacts").await?;
    let log_bytes = scalar_sum(pool, "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM logs").await?;

    // 无匹配 agent（no_online_agent / missing_labels）才是「构建无法下发」；
    // no_slot 是「有匹配 agent 但无空槽」——调度仍会推进，不该点亮警示
    // （spec 断言：警示语义 = 匹配不上）。
    let has_no_match = queue
        .iter()
        .any(|(reason, depth)| *depth > 0 && matches!(reason, &"no_online_agent" | &"missing_labels"));

    Ok(Snapshot {
        queue,
        agents_online: agents.0,
        agents_total: agents.1,
        slots_used: slots.0,
        slots_total: slots.1,
        builds_terminal,
        artifact_bytes,
        log_bytes,
        has_no_match,
        has_offline_agent: agents.2,
        has_draining_incompatible: agents.3,
    })
}

/// 队列深度按原因分类：全部 queued 任务（含未组装 spec 的——等待面即整条
/// 排队链，不只看可下发池）。按 `waiting_detail` 前缀归类。
async fn queue_by_reason(pool: &SqlitePool) -> Result<BTreeMap<&'static str, u64>, StoreError> {
    let rows: Vec<Option<String>> =
        sqlx::query_scalar("SELECT waiting_detail FROM jobs WHERE status = 'queued'")
            .fetch_all(pool)
            .await?;
    let mut map: BTreeMap<&'static str, u64> = BTreeMap::new();
    for detail in rows {
        let key = classify(detail.as_deref());
        *map.entry(key).or_insert(0) += 1;
    }
    Ok(map)
}

/// 等待详情 → 固定原因标签（前缀匹配，缺标签详情可变）。
fn classify(detail: Option<&str>) -> &'static str {
    match detail {
        Some(d) if d.starts_with(WAIT_NO_ONLINE) => "no_online_agent",
        Some(d) if d.starts_with(WAIT_MISSING_TAG) => "missing_labels",
        Some(d) if d.starts_with(WAIT_NO_SLOT) => "no_slot",
        _ => WAIT_UNCATEGORIZED,
    }
}

/// Agent 统计：(在线数, 总数, 有启用但离线, 有排空/不兼容)。
/// 停用 Agent 离线是预期态（不计离线警示）；排空/不兼容 = 在线但
/// [`AgentRow::mid_upgrade`]/[`AgentRow::version_incompatible`]（ADR-0017 四态）。
async fn agent_stats(pool: &SqlitePool) -> Result<(u64, u64, bool, bool), StoreError> {
    let agents = crate::store::agents::AgentRepo::new(pool.clone()).list().await?;
    let total = agents.len() as u64;
    let online = agents.iter().filter(|a| a.online).count() as u64;
    let has_offline = agents
        .iter()
        .any(|a| !a.online && !a.disabled);
    let server = crate::store::agents::AgentRepo::new(pool.clone()).server_version();
    let mut has_draining = false;
    for agent in &agents {
        if !agent.online || agent.disabled {
            continue;
        }
        if agent.mid_upgrade() || agent.version_incompatible(&server)? {
            has_draining = true;
            break;
        }
    }
    Ok((online, total, has_offline, has_draining))
}

/// 槽位统计：(占用, 总量)。占用 = running/unknown 在途任务总数；总量 =
/// 全部在线 Agent 的 max_concurrency 之和（离线 Agent 不供槽）。
async fn slot_stats(pool: &SqlitePool) -> Result<(u64, u64), StoreError> {
    let used: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status IN ('running', 'unknown')")
            .fetch_one(pool)
            .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(max_concurrency), 0) FROM agents WHERE online = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok((used as u64, total as u64))
}

/// 构建终态计数（succeeded/failed/cancelled/timeout）。
async fn terminal_counts(pool: &SqlitePool) -> Result<BTreeMap<String, u64>, StoreError> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, COUNT(*) FROM builds WHERE status IN ('succeeded', 'failed', 'cancelled', 'timeout') GROUP BY status")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(status, count)| (status, count as u64))
        .collect())
}

/// 单值聚合（`SUM`/`LENGTH` 类；空表 COALESCE 为 0）。
async fn scalar_sum(pool: &SqlitePool, sql: &'static str) -> Result<u64, StoreError> {
    let value: i64 = sqlx::query_scalar(sql).fetch_one(pool).await?;
    Ok(value.max(0) as u64)
}

/// 最近构建（跨项目，按终态/更新时间倒序取前 [`RECENT_BUILDS_LIMIT`]）。
/// `visible_projects` 为调用者可见的项目集（全局 admin 全量、普通用户按
/// 成员过滤）——构建行按项目名归拢，可见性在 API 层（或本函数入参）裁决。
pub async fn recent_builds(
    pool: &SqlitePool,
    visible: &[Project],
    limit: usize,
) -> Result<Vec<RecentBuild>, StoreError> {
    if visible.is_empty() {
        return Ok(Vec::new());
    }
    // 收集可见项目的构建行，按 finished_at（终态）/ started_at / updated_at
    // 倒序取前 limit——「最近构建」的语义是最近有动静的构建。
    let mut builds: Vec<(Project, BuildRow)> = Vec::new();
    for project in visible {
        let rows = BuildRepo::new(pool.clone())
            .list_by_project(project.id)
            .await?;
        builds.extend(rows.into_iter().map(|row| (project.clone(), row)));
    }
    builds.sort_by(|a, b| {
        let a_key = recency_key(&a.1);
        let b_key = recency_key(&b.1);
        b_key.cmp(&a_key)
    });
    Ok(builds
        .into_iter()
        .take(limit)
        .map(|(project, row)| RecentBuild::from_row(&project.name, &row))
        .collect())
}

/// 最近活动键：终态时刻 > 开始时刻 > 更新时刻 > 0（倒序即「最近有动静」）。
fn recency_key(row: &BuildRow) -> i64 {
    row.finished_at
        .or(row.started_at)
        .or(Some(row.updated_at))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};

    /// 临时库 + 项目行（store 缝测试形态）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时数据目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path()).await.expect("bootstrap");
        ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        (dir, pool)
    }

    #[test]
    fn classify_maps_waiting_details_to_fixed_reasons() {
        assert_eq!(
            classify(Some("等待匹配 agent：无在线 agent")),
            "no_online_agent"
        );
        assert_eq!(
            classify(Some("等待匹配 agent：缺标签 gpu=nvidia")),
            "missing_labels"
        );
        assert_eq!(
            classify(Some("等待匹配 agent：在线 agent 无空槽")),
            "no_slot"
        );
        assert_eq!(classify(Some("下发失败：x")), "uncategorized");
        assert_eq!(classify(None), "uncategorized");
    }

    /// 空库快照：全部零值 + 无警示态（stat 卡全量真值的空态）。
    #[tokio::test]
    async fn compute_empty_is_all_zero() {
        let (_dir, pool) = fixture().await;
        let snap = compute(&pool).await.expect("快照");
        assert_eq!(snap.queue_depth(), 0);
        assert_eq!(snap.agents_online, 0);
        assert_eq!(snap.agents_total, 0);
        assert_eq!(snap.slots_used, 0);
        assert_eq!(snap.slots_total, 0);
        assert_eq!(snap.artifact_bytes, 0);
        assert_eq!(snap.log_bytes, 0);
        assert!(!snap.has_no_match);
        assert!(!snap.has_offline_agent);
        assert!(!snap.has_draining_incompatible);
    }

    /// 队列深度 + 原因分类（直插 queued 任务行，waiting_detail 各异）。
    #[tokio::test]
    async fn compute_aggregates_queue_by_reason() {
        let (_dir, pool) = fixture().await;
        let build_id = insert_build(&pool, 1, "release", 1).await;
        for (i, detail) in [
            Some("等待匹配 agent：缺标签 gpu=nvidia"),
            Some("等待匹配 agent：缺标签 gpu=nvidia"),
            Some("等待匹配 agent：无在线 agent"),
            Some("等待匹配 agent：在线 agent 无空槽"),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            insert_job(&pool, build_id, &format!("j{i}"), "queued", detail).await;
        }

        let snap = compute(&pool).await.expect("快照");
        assert_eq!(snap.queue_depth(), 5, "全部 queued 计入");
        assert_eq!(snap.queue.get("missing_labels"), Some(&2));
        assert_eq!(snap.queue.get("no_online_agent"), Some(&1));
        assert_eq!(snap.queue.get("no_slot"), Some(&1));
        assert_eq!(snap.queue.get("uncategorized"), Some(&1));
        assert!(snap.has_no_match, "存在无匹配原因任务 → 警示");
    }

    /// 警示语义：no_slot（有匹配 agent 但无空槽）不算「无匹配」——调度仍会
    /// 推进，不该点亮 `has_no_match`（spec：警示 = 匹配不上，票 B5-T7）。
    #[tokio::test]
    async fn compute_no_slot_alone_does_not_flag_no_match() {
        let (_dir, pool) = fixture().await;
        let build_id = insert_build(&pool, 1, "release", 1).await;
        insert_job(&pool, build_id, "j0", "queued", Some("等待匹配 agent：在线 agent 无空槽"))
            .await;

        let snap = compute(&pool).await.expect("快照");
        assert_eq!(snap.queue.get("no_slot"), Some(&1));
        assert!(!snap.has_no_match, "仅 no_slot 排队不点亮无匹配警示");
    }

    /// 警示态：离线 Agent / 排空 Agent 推导（停用 Agent 离线不计）。
    #[tokio::test]
    async fn compute_detects_offline_and_draining_alerts() {
        let (_dir, pool) = fixture().await;
        let agents = crate::store::agents::AgentRepo::new(pool.clone());
        // 启用但离线 → has_offline_agent。
        let offline = agents
            .create(crate::store::agents::NewAgent {
                name: "linux-offline".into(),
                token_hash: "sisa-hash-offline".into(),
                system_labels: "[]".into(),
                custom_labels: "[]".into(),
                max_concurrency: 1,
                register_code_hash: "code-hash-offline".into(),
                register_code_expires_at: 1_700_000_000_000,
            })
            .await
            .expect("建 Agent");
        let snap = compute(&pool).await.expect("快照");
        assert!(snap.has_offline_agent, "启用但离线 → 警示");
        assert_eq!(snap.agents_total, 1);
        assert_eq!(snap.agents_online, 0);

        // 停用 Agent 离线不计（预期态）。
        agents
            .set_disabled(offline.id, true)
            .await
            .expect("停用");
        let snap = compute(&pool).await.expect("快照");
        assert!(!snap.has_offline_agent, "停用离线是预期态");
        assert_eq!(snap.agents_total, 1, "总数含停用");

        // 排空 Agent（在线 + pending_upgrade）→ has_draining_incompatible。
        agents
            .set_disabled(offline.id, false)
            .await
            .expect("启用");
        agents
            .mark_online(offline.id, "[]", None, 1_000)
            .await
            .expect("上线");
        agents
            .set_pending_upgrade(
                offline.id,
                &crate::store::agents::PendingUpgrade {
                    package_name: "sisyphus-agent-1.0.0-linux-x86_64.tar.gz".into(),
                    sha256: "abc".into(),
                    download_url: "/api/v1/agent/upgrade-packages/x".into(),
                },
            )
            .await
            .expect("待升级");
        let snap = compute(&pool).await.expect("快照");
        assert!(snap.has_draining_incompatible, "排空 Agent → 警示");
        assert_eq!(snap.agents_online, 1);
    }

    /// 构建终态计数 + 产物/日志字节占用。
    #[tokio::test]
    async fn compute_counts_terminals_and_storage_bytes() {
        let (_dir, pool) = fixture().await;
        let b1 = insert_build(&pool, 1, "release", 1).await;
        let b2 = insert_build(&pool, 1, "release", 2).await;
        set_build_status(&pool, b1, "succeeded").await;
        set_build_status(&pool, b2, "failed").await;

        // 产物元数据（size 求和）+ 日志 chunk（压缩体 LENGTH 求和）。
        sqlx::query(
            "INSERT INTO artifacts (build_id, name, path, size, sha256, created_at, retention_until)
             VALUES (?, 'a.bin', '1/a.bin', 100, 'x', 0, 1)",
        )
        .bind(b1)
        .execute(&pool)
        .await
        .expect("产物");
        sqlx::query(
            "INSERT INTO artifacts (build_id, name, path, size, sha256, created_at, retention_until)
             VALUES (?, 'b.bin', '1/b.bin', 250, 'y', 0, 1)",
        )
        .bind(b1)
        .execute(&pool)
        .await
        .expect("产物");
        let job_id = insert_job(&pool, b1, "compile", "succeeded", None).await;
        sqlx::query(
            "INSERT INTO logs (build_id, job_id, attempt, start_seq, end_seq, step, stream, data, created_at)
             VALUES (?, ?, 1, 0, 0, -1, '', X'1f8b', 0)",
        )
        .bind(b1)
        .bind(job_id)
        .execute(&pool)
        .await
        .expect("日志");

        let snap = compute(&pool).await.expect("快照");
        assert_eq!(snap.builds_terminal.get("succeeded"), Some(&1));
        assert_eq!(snap.builds_terminal.get("failed"), Some(&1));
        assert_eq!(snap.artifact_bytes, 350, "产物 size 求和");
        assert_eq!(snap.log_bytes, 2, "gzip 魔数 X'1f8b' = 2 字节");
    }

    /// 最近构建：跨项目倒序 + 可见性过滤 + limit。
    #[tokio::test]
    async fn recent_builds_lists_by_activity_desc() {
        let (_dir, pool) = fixture().await;
        ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "other".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/other".into(),
                default_branch: None,
            })
            .await
            .expect("建 other");
        let projects = ProjectRepo::new(pool.clone()).list().await.expect("清单");
        let demo = projects.iter().find(|p| p.name == "demo").expect("demo");
        let other = projects.iter().find(|p| p.name == "other").expect("other");

        // demo 两条：d1 终态（finished 2000）、d2 运行中（updated 1002）。
        // other 一条：o1 终态（finished 3000，最晚）。
        let d1 = insert_build(&pool, demo.id, "release", 1).await;
        let d2 = insert_build(&pool, demo.id, "release", 2).await;
        let o1 = insert_build(&pool, other.id, "build", 1).await;
        set_build_status(&pool, d1, "succeeded").await;
        set_build_status_at(&pool, o1, "succeeded", 3_000).await;

        // 只给 demo 可见（其他项目不可见）。
        let visible = vec![demo.clone()];
        let recent = recent_builds(&pool, &visible, 10).await.expect("最近");
        assert_eq!(recent.len(), 2, "只列可见项目构建");
        assert_eq!(recent[0].number, 1, "d1 终态（2000）先于运行中 d2（1002）");
        assert_eq!(recent[1].number, 2);
        assert_eq!(recent[0].status, "succeeded");

        // 全量可见：o1 终态（3000）最晚 → 排最前。
        let visible = vec![demo.clone(), other.clone()];
        let recent = recent_builds(&pool, &visible, 10).await.expect("最近");
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].project, "other", "o1 终态最晚排最前");

        // limit 生效。
        let recent = recent_builds(&pool, &visible, 1).await.expect("最近");
        assert_eq!(recent.len(), 1);
        let _ = d2;
    }

    // -----------------------------------------------------------------------
    // 测试夹具
    // -----------------------------------------------------------------------

    /// 直插构建行（queued），返回 id。
    async fn insert_build(pool: &SqlitePool, project_id: i64, pipeline: &str, number: i64) -> i64 {
        sqlx::query(
            "INSERT INTO builds (project_id, pipeline_name, number, status, trigger, trigger_detail, attempt, snapshot, updated_at)
             VALUES (?, ?, ?, 'queued', 'manual', '{}', 1, '{}', ?)",
        )
        .bind(project_id)
        .bind(pipeline)
        .bind(number)
        .bind(1_000 + number)
        .execute(pool)
        .await
        .expect("建构建")
        .last_insert_rowid()
    }

    async fn set_build_status(pool: &SqlitePool, id: i64, status: &str) {
        set_build_status_at(pool, id, status, 2_000).await
    }

    async fn set_build_status_at(pool: &SqlitePool, id: i64, status: &str, at: i64) {
        sqlx::query("UPDATE builds SET status = ?, finished_at = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(at)
            .bind(at)
            .bind(id)
            .execute(pool)
            .await
            .expect("置状态");
    }

    async fn insert_job(
        pool: &SqlitePool,
        build_id: i64,
        name: &str,
        status: &str,
        waiting: Option<&str>,
    ) -> i64 {
        sqlx::query(
            "INSERT INTO jobs (build_id, stage_index, name, status, attempt, labels, timeout_minutes, retry_count, allow_failure, waiting_detail)
             VALUES (?, 0, ?, ?, 1, '[]', 0, 0, 0, ?)",
        )
        .bind(build_id)
        .bind(name)
        .bind(status)
        .bind(waiting)
        .execute(pool)
        .await
        .expect("建任务")
        .last_insert_rowid()
    }
}

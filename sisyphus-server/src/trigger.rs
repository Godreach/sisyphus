//! trigger 模块：cron + poll 触发源（票 B2c-T6，ADR-0016，消费 #46 engine 统一
//! 触发入口 + #48 调度面）。
//!
//! - **cron**：按表达式节奏扫表，命中即触发构建（默认值 + 默认分支 head，
//!   不钉 commit——Agent 执行期检分支头）。`cron` crate 解析 5 字段表达式
//!   （内部前缀 `"0 "` 凑 6 字段秒位），[`Schedule::after`] 求 `(last, now]`
//!   区间命中点，取最晚一个触发一次（多 missed 不补跑多份），`last_probe_at`
//!   记命中点去重（同命中点不重触发）。
//! - **poll**：按项目节奏轮询（默认 5 分钟，config `[triggers]`），探测经
//!   [`crate::scm::ScmProbe`] trait 缝隔离。创建/启用时记基线不触发、之后只
//!   对新提交触发、commit-id 去重；探测失败记 `last_probe_error`、按节奏
//!   重试、不自动禁用（ADR-0016）。
//!
//! 测试以假探测（[`crate::scm::FakeProbe`]）+ 假时钟（`now` 注入 [`Self::tick`]
//! ）驱动基线/去重/节奏/失败历史断言，不依赖真实 sleep（避免 flaky）。

use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::engine::{Engine, StartBuildInput, TriggerDetail};
use crate::scm::ScmProbe;
use crate::store::builds::TriggerSource;
use crate::store::projects::{ProjectRepo, ScmType};
use crate::store::triggers::{TriggerKind, TriggerRepo, TriggerRow};
use crate::store::{StoreError, now_ms};

/// cron 命中点回看上限（`(last, now]` 区间最多迭代多少个命中点）。防
/// 「高频 cron + 长停机」无界迭代（如每分钟 cron 停机一年）；正常 hourly/
/// daily cron 在第一个 `fire > now` 即 break。命中上限取最晚一个触发一次。
const MAX_CRON_CATCHUP: usize = 10_000;

/// 后台扫描循环周期（生产 [`TriggerEngine::run`]）：cron 分钟粒度，30s 内
/// 必扫到当分钟命中点；poll 到期判定由各触发器 spec 节奏裁剪，不在此周期
/// 叠加忙轮（30s 扫表成本可忽略，triggers 表小）。
pub const TRIGGER_LOOP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// spec 解析/校验错误（REST 422 映射；trigger 引擎内部按「坏 spec 跳过」处理）。
#[derive(Debug)]
pub enum SpecError {
    /// spec JSON 解码失败。
    Json(String),
    /// cron 表达式非法（非 5 字段 / `cron` crate 解析失败）。
    Cron(String),
    /// poll 节奏非法（< 1 分钟）。
    Interval(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "触发器 spec JSON 非法：{e}"),
            Self::Cron(e) => write!(f, "cron 表达式非法：{e}"),
            Self::Interval(e) => write!(f, "poll 节奏非法：{e}"),
        }
    }
}

impl std::error::Error for SpecError {}

/// cron 触发器 spec（解析自 `triggers.spec` JSON `{"expr":"..."}`）。
///
/// `expr` 为 5 字段 cron 表达式（`minute hour day-of-month month day-of-week`，
/// 标准 Unix/CI 形态）。内部前缀 `"0 "` 凑 6 字段秒位供 `cron` crate 解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    /// 5 字段 cron 表达式。
    pub expr: String,
}

/// poll 触发器 spec（解析自 `triggers.spec` JSON `{"interval_minutes":N}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollSpec {
    /// 轮询节奏（分钟，>= 1；默认值取 config `[triggers] poll_interval_minutes`）。
    pub interval_minutes: i64,
}

impl CronSpec {
    /// 从 `triggers.spec` JSON 文本解析 + 校验（5 字段 + `cron` crate 可解析）。
    pub fn parse(spec: &str) -> Result<Self, SpecError> {
        let parsed: Self =
            serde_json::from_str(spec).map_err(|e| SpecError::Json(e.to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// 校验已构造的 spec（5 字段 + `cron` crate 可解析）——REST 层组装后直接
    /// 调，免再经 JSON 序列化→parse 往返。
    pub fn validate(&self) -> Result<(), SpecError> {
        parse_cron_schedule(&self.expr).map_err(SpecError::Cron)?;
        Ok(())
    }

    /// 序列化为 `triggers.spec` JSON 文本。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("CronSpec JSON 序列化恒成功")
    }
}

impl PollSpec {
    /// 从 `triggers.spec` JSON 文本解析 + 校验（节奏 >= 1 分钟）。
    pub fn parse(spec: &str) -> Result<Self, SpecError> {
        let parsed: Self =
            serde_json::from_str(spec).map_err(|e| SpecError::Json(e.to_string()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// 校验已构造的 spec（节奏 >= 1 分钟）——REST 层组装后直接调，免 JSON 往返。
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.interval_minutes < 1 {
            return Err(SpecError::Interval(format!(
                "poll 节奏须 >= 1 分钟，得到 {}",
                self.interval_minutes
            )));
        }
        Ok(())
    }

    /// 序列化为 `triggers.spec` JSON 文本。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("PollSpec JSON 序列化恒成功")
    }

    /// 节奏毫秒（>= 60_000）。
    fn interval_ms(&self) -> i64 {
        self.interval_minutes.max(1) * 60_000
    }
}

/// 解析 5 字段 cron 表达式为 `cron::Schedule`（内部前缀 `"0 "` 凑 6 字段秒位）。
pub(crate) fn parse_cron_schedule(expr: &str) -> Result<Schedule, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron 表达式须 5 字段（分 时 日 月 周），得到 {} 字段：{expr:?}",
            fields.len()
        ));
    }
    let six = format!("0 {expr}");
    Schedule::from_str(&six).map_err(|e| format!("cron 表达式解析失败：{e}"))
}

/// 单次扫描报告（测试断言用：各类触发/探测计数）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// cron 触发构建次数。
    pub cron_fired: usize,
    /// poll 触发构建次数（新提交）。
    pub poll_fired: usize,
    /// poll 记基线不触发次数（首探/启用后首探）。
    pub poll_baseline: usize,
    /// poll 去重次数（head == baseline，无新提交）。
    pub poll_no_change: usize,
    /// poll 空仓库次数（探测无提交）。
    pub poll_no_commit: usize,
    /// 探测/触发失败次数（记 `last_probe_error`，按节奏重试）。
    pub probe_errors: usize,
    /// 坏 spec 跳过次数。
    pub spec_skipped: usize,
}

/// 触发引擎：cron 扫表 + poll 轮询，调 [`Engine::start_build`]。
///
/// 组合根装配：engine（编排推进，与 REST/sched 共享同一引擎与事件总线）
/// 与探测端口（生产 [`crate::scm::SystemScmProbe`]，测试
/// [`crate::scm::FakeProbe`]）。后台 [`Self::run`] 周期调 [`Self::tick`]；
/// 测试直接调 [`Self::tick`] 注入假时钟。
pub struct TriggerEngine {
    engine: Engine,
    triggers: TriggerRepo,
    projects: ProjectRepo,
    probe: Arc<dyn ScmProbe>,
}

impl TriggerEngine {
    /// 装配：engine（编排推进）+ 探测端口（trait 缝）。
    pub fn new(engine: Engine, pool: SqlitePool, probe: Arc<dyn ScmProbe>) -> Self {
        Self {
            engine,
            triggers: TriggerRepo::new(pool.clone()),
            projects: ProjectRepo::new(pool),
            probe,
        }
    }

    /// 单次扫描：处理全部启用触发器。`now` 注入（测试假时钟；生产
    /// [`now_ms`]）。逐触发器独立处理——单个的存储错误记日志不中断全扫。
    pub async fn tick(&self, now: i64) -> Result<TickReport, StoreError> {
        let mut report = TickReport::default();
        let enabled = self.triggers.list_enabled().await?;
        for t in enabled {
            let result = match t.kind {
                TriggerKind::Cron => self.tick_cron(&t, now, &mut report).await,
                TriggerKind::Poll => self.tick_poll(&t, now, &mut report).await,
            };
            if let Err(e) = result {
                tracing::warn!(trigger_id = t.id, error = %e, "触发器扫描失败");
            }
        }
        Ok(report)
    }

    /// cron 扫表：求 `(last, now]` 区间命中点，取最晚一个触发构建。
    async fn tick_cron(
        &self,
        t: &TriggerRow,
        now: i64,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let spec = match CronSpec::parse(&t.spec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(trigger_id = t.id, error = %e, "cron spec 非法，跳过");
                report.spec_skipped += 1;
                return Ok(());
            }
        };
        let schedule = match parse_cron_schedule(&spec.expr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(trigger_id = t.id, error = %e, "cron 表达式非法，跳过");
                report.spec_skipped += 1;
                return Ok(());
            }
        };
        // 命中锚点：上次命中点（去重用），缺省回溯到创建时刻（不追溯建表前
        // 的命中——从下一个命中点起触发）。
        let last = t.last_probe_at.unwrap_or(t.created_at);
        let Some(now_dt) = DateTime::<Utc>::from_timestamp_millis(now) else {
            return Ok(());
        };
        let Some(last_dt) = DateTime::<Utc>::from_timestamp_millis(last) else {
            return Ok(());
        };
        let mut due: Option<DateTime<Utc>> = None;
        let mut count = 0usize;
        for fire in schedule.after(&last_dt) {
            if fire <= now_dt {
                due = Some(fire);
            } else {
                break;
            }
            count += 1;
            if count >= MAX_CRON_CATCHUP {
                break;
            }
        }
        let Some(due) = due else {
            return Ok(());
        };
        let Some(project) = self.projects.get_by_id(t.project_id).await? else {
            return Ok(()); // 项目缺失（无删除端点，理论上不发生）
        };
        self.engine
            .start_build(StartBuildInput {
                project_name: project.name.clone(),
                pipeline_name: t.pipeline_name.clone(),
                trigger: TriggerSource::Cron,
                detail: TriggerDetail {
                    by: "cron".into(),
                    branch: project.default_branch.clone(),
                    commit: None, // 默认分支 head：Agent 执行期检
                    revision: None,
                    params: vec![], // 默认值
                },
            })
            .await?;
        // 记命中点去重（同命中点不重触发）。
        self.triggers
            .record_probe(t.id, due.timestamp_millis(), None)
            .await?;
        report.cron_fired += 1;
        Ok(())
    }

    /// poll 轮询：到期则探测，基线/去重/失败历史（ADR-0016）。
    async fn tick_poll(
        &self,
        t: &TriggerRow,
        now: i64,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let spec = match PollSpec::parse(&t.spec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(trigger_id = t.id, error = %e, "poll spec 非法，跳过");
                report.spec_skipped += 1;
                return Ok(());
            }
        };
        // 到期判定：从未探测（last_probe_at 缺省 0）→ 立即到期（首探记基线）。
        let last = t.last_probe_at.unwrap_or(0);
        if now - last < spec.interval_ms() {
            return Ok(()); // 未到期
        }
        let Some(project) = self.projects.get_by_id(t.project_id).await? else {
            return Ok(());
        };
        match self.probe.probe_head(&project).await {
            Err(e) => {
                // 探测失败：记历史、按节奏重试、不自动禁用（ADR-0016）。
                self.triggers.record_probe(t.id, now, Some(&e)).await?;
                report.probe_errors += 1;
                tracing::warn!(trigger_id = t.id, error = %e, "poll 探测失败（记历史，按节奏重试）");
                Ok(())
            }
            Ok(None) => {
                // 空仓库：记探测成功，不触发；baseline 不动（无提交）。
                self.triggers.record_probe(t.id, now, None).await?;
                report.poll_no_commit += 1;
                Ok(())
            }
            Ok(Some(head)) => match &t.baseline_commit {
                None => {
                    // 首探/启用后首探：记基线，不触发（ADR-0016）。
                    self.triggers.record_baseline(t.id, &head, now).await?;
                    report.poll_baseline += 1;
                    Ok(())
                }
                Some(baseline) if baseline == &head => {
                    // commit-id 去重：head 未变，不触发。
                    self.triggers.record_probe(t.id, now, None).await?;
                    report.poll_no_change += 1;
                    Ok(())
                }
                Some(_) => {
                    // 新提交：触发构建 + 更新基线。git→commit、svn→revision。
                    let (commit, revision) = match project.scm_type {
                        ScmType::Git => (Some(head.clone()), None),
                        ScmType::Svn => (None, Some(head.clone())),
                    };
                    match self
                        .engine
                        .start_build(StartBuildInput {
                            project_name: project.name.clone(),
                            pipeline_name: t.pipeline_name.clone(),
                            trigger: TriggerSource::Poll,
                            detail: TriggerDetail {
                                by: "poll".into(),
                                branch: project.default_branch.clone(),
                                commit,
                                revision,
                                params: vec![],
                            },
                        })
                        .await
                    {
                        Ok(_) => {
                            self.triggers.record_baseline(t.id, &head, now).await?;
                            report.poll_fired += 1;
                            Ok(())
                        }
                        Err(e) => {
                            // 触发构建失败（如 pipeline 缺失）：记入触发器历史、
                            // 不更新基线 → 下次节奏对新提交重试（与探测失败同处理面）。
                            self.triggers
                                .record_probe(t.id, now, Some(&format!("触发构建失败：{e}")))
                                .await?;
                            report.probe_errors += 1;
                            Ok(())
                        }
                    }
                }
            },
        }
    }

    /// 周期循环（生产 tokio 任务）：按 `interval` 节奏周期调 [`Self::tick`]
    /// （`now = now_ms()`）。运行期不返回（进程级生命周期），错误记日志续跑。
    pub async fn run(self, interval: std::time::Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick(now_ms()).await {
                tracing::warn!(error = %e, "触发器扫描循环失败");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::scm::FakeProbe;
    use crate::secrets::MasterKey;
    use crate::store::builds::{BuildRepo, TriggerSource};
    use crate::store::pipelines::PipelineRepo;
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};
    use crate::store::triggers::{TriggerInput, TriggerKind};
    use sisyphus_model::pipeline::Pipeline;
    use std::sync::Arc;

    /// 独立临时目录 + 已迁移库 + 项目 demo（git，默认分支 main）+ 触发引擎。
    /// 返回 pool 供断言直读（builds/triggers repo 共池）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool, TriggerEngine) {
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
        let engine = Engine::new(pool.clone(), MasterKey::generate(), EventBus::new());
        let trigger = TriggerEngine::new(
            engine,
            pool.clone(),
            Arc::new(FakeProbe::new()) as Arc<dyn ScmProbe>,
        );
        (dir, pool, trigger)
    }

    /// 单阶段单任务最小 pipeline（含一个默认值参数），足够触发 + 组装验证。
    fn pipeline() -> Pipeline {
        use sisyphus_model::pipeline::{
            EnvVar, Job, Parameter, ParameterType, ParameterValue, Stage, Step,
        };
        Pipeline {
            name: "release".into(),
            parameters: vec![Parameter {
                name: "target".into(),
                r#type: ParameterType::String,
                required: false,
                default: Some(ParameterValue::String("x86_64".into())),
                description: None,
                choices: vec![],
            }],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec![],
                    when: None,
                    env: vec![EnvVar {
                        name: "MODE".into(),
                        value: "${target}".into(),
                    }],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "echo ${SISY_BUILD_NUMBER}".into(),
                        shell: None,
                        when: None,
                    }],
                }],
            }],
            revision: None,
        }
    }

    /// 保存 release 定义 + 建 git 项目（fixture 未建项目时用）。
    async fn save_project_and_pipeline(pool: &SqlitePool, scm: ScmType, branch: Option<&str>) {
        ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: scm,
                scm_url: "https://example.com/repo".into(),
                default_branch: branch.map(str::to_string),
            })
            .await
            .expect("建项目");
        PipelineRepo::new(pool.clone())
            .save("demo", "release", &pipeline(), "tester")
            .await
            .expect("保存定义");
    }

    /// 建 cron 触发器（fixture 已建项目 + 定义时用），返回行。
    async fn create_cron(pool: &SqlitePool, expr: &str, enabled: bool) -> TriggerRow {
        TriggerRepo::new(pool.clone())
            .create(TriggerInput {
                project_id: 1,
                pipeline_name: "release".into(),
                kind: TriggerKind::Cron,
                spec: CronSpec { expr: expr.into() }.to_json(),
                enabled,
            })
            .await
            .expect("建 cron")
    }

    /// 取 release 的全部构建号（按号序）。
    async fn builds(pool: &SqlitePool) -> Vec<i64> {
        BuildRepo::new(pool.clone())
            .list_page(1, "release", None, 100, 0)
            .await
            .expect("列构建")
            .into_iter()
            .map(|r| r.number)
            .collect()
    }

    /// poll 测试时间基线（远大于节奏，避开「never probed → 0」兜底与真实
    /// created_at 的非确定性；假时钟从此起按节奏步进）。
    const T0: i64 = 10_000_000;
    /// 1 分钟毫秒（节奏步进单位）。
    const MIN: i64 = 60_000;

    /// 钉 cron 锚点（`last_probe_at`）为可控值——绕开 `created_at` 真实时非
    /// 确定性，直接测「给定锚点，区间 (last, now] 命中」的匹配逻辑。
    async fn prime_anchor(repo: &TriggerRepo, id: i64, at: i64) {
        repo.record_probe(id, at, None).await.expect("钉锚点");
    }

    // ---- cron ----

    /// AC：cron 按表达式匹配时间、默认分支 head 触发构建、启停生效；命中点去重。
    #[tokio::test]
    async fn cron_fires_at_matching_time_with_default_branch() {
        let (dir, pool, trigger) = fixture().await;
        save_project_and_pipeline(&pool, ScmType::Git, Some("main")).await;
        let cron = create_cron(&pool, "0 2 * * *", true).await;
        // 钉锚点 0（epoch）——绕开 created_at 真实时，直接测匹配逻辑。
        prime_anchor(&TriggerRepo::new(pool.clone()), cron.id, 0).await;

        // 02:00:00 UTC 命中（区间 (0, 02:00] 含 1970-01-01 02:00）。
        let two_am = 2 * 3600 * 1000;
        let report = trigger.tick(two_am).await.expect("扫描");
        assert_eq!(report.cron_fired, 1, "02:00 命中应触发一次");
        assert_eq!(builds(&pool).await, vec![1]);

        // 触发上下文：默认分支 main、不钉 commit、默认参数。
        let build = BuildRepo::new(pool.clone())
            .get_by_number(1, "release", 1)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.trigger, TriggerSource::Cron);
        let detail: TriggerDetail = serde_json::from_str(&build.trigger_detail).expect("解析");
        assert_eq!(detail.by, "cron");
        assert_eq!(detail.branch.as_deref(), Some("main"));
        assert_eq!(detail.commit, None, "cron 不钉 commit（Agent 检分支头）");
        assert!(detail.params.is_empty(), "默认值");

        // 命中点已记 → 同一 02:00 再扫不重触发（去重）。
        let report = trigger.tick(two_am).await.expect("再扫");
        assert_eq!(report.cron_fired, 0, "同命中点不重触发");
        assert_eq!(builds(&pool).await.len(), 1);
        let _ = (dir, cron);
    }

    /// AC：cron 未到命中点不触发。
    #[tokio::test]
    async fn cron_does_not_fire_before_match() {
        let (_dir, pool, trigger) = fixture().await;
        save_project_and_pipeline(&pool, ScmType::Git, Some("main")).await;
        let cron = create_cron(&pool, "0 2 * * *", true).await;
        prime_anchor(&TriggerRepo::new(pool.clone()), cron.id, 0).await;
        // 01:59:30 —— 02:00 未到，区间 (0, 01:59:30] 无 02:00 命中。
        let before = (2 * 3600 - 30) * 1000;
        let report = trigger.tick(before).await.expect("扫描");
        assert_eq!(report.cron_fired, 0);
        assert!(builds(&pool).await.is_empty());
    }

    /// AC：cron 启停生效——停用后扫表不命中。
    #[tokio::test]
    async fn cron_disabled_does_not_fire() {
        let (_dir, pool, trigger) = fixture().await;
        save_project_and_pipeline(&pool, ScmType::Git, Some("main")).await;
        let cron = create_cron(&pool, "0 2 * * *", true).await;
        let repo = TriggerRepo::new(pool.clone());
        repo.update(cron.id, None, Some(false)).await.expect("停用");
        // 停用后不在 list_enabled，钉锚点也无济——扫表根本不触达。
        let report = trigger.tick(2 * 3600 * 1000).await.expect("扫描");
        assert_eq!(report.cron_fired, 0, "停用触发器不在 list_enabled");
        assert!(builds(&pool).await.is_empty());
    }

    /// AC：cron 多 missed 不补跑多份——区间含多个命中点时只触发最晚一个。
    #[tokio::test]
    async fn cron_catchup_fires_once_for_latest_missed() {
        let (_dir, pool, trigger) = fixture().await;
        save_project_and_pipeline(&pool, ScmType::Git, Some("main")).await;
        let cron = create_cron(&pool, "0 * * * *", true).await; // 每小时整点。
        prime_anchor(&TriggerRepo::new(pool.clone()), cron.id, 0).await;
        // now = 3h5m → 区间 (0, 3h5m] 含 1h/2h/3h 三个命中点，取最晚 3h 触发一次。
        let now = (3 * 3600 + 5 * 60) * 1000;
        let report = trigger.tick(now).await.expect("扫描");
        assert_eq!(report.cron_fired, 1, "多 missed 只触发一次（最晚命中点）");
        assert_eq!(builds(&pool).await.len(), 1);
    }

    // ---- poll 基线 / 去重 / 节奏 / 失败历史 ----

    /// 已建项目（git/svn）+ 定义 + poll 触发器 + 可控探测，返回 (pool, trigger,
    /// poll_row, probe)。
    async fn poll_fixture(
        scm: ScmType,
        branch: Option<&str>,
        interval_minutes: i64,
    ) -> (SqlitePool, TriggerEngine, TriggerRow, Arc<FakeProbe>) {
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
        save_project_and_pipeline(&pool, scm, branch).await;
        let engine = Engine::new(pool.clone(), MasterKey::generate(), EventBus::new());
        let probe = Arc::new(FakeProbe::new());
        let trigger = TriggerEngine::new(engine, pool.clone(), probe.clone() as Arc<dyn ScmProbe>);
        let poll = TriggerRepo::new(pool.clone())
            .create(TriggerInput {
                project_id: 1,
                pipeline_name: "release".into(),
                kind: TriggerKind::Poll,
                spec: PollSpec { interval_minutes }.to_json(),
                enabled: true,
            })
            .await
            .expect("建 poll");
        // TempDir 随测试存活：存进静态丢弃槽防过早清理。
        LEAK_DIR.lock().expect("leak slot").push(dir);
        (pool, trigger, poll, probe)
    }

    // TempDir 必须活到测试结束才清理；poll_fixture 把目录存进此静态槽
    // （poll 用例不绑定局部 _dir——用例只需 pool/probe）。
    static LEAK_DIR: std::sync::Mutex<Vec<tempfile::TempDir>> = std::sync::Mutex::new(Vec::new());

    /// AC：创建/启用时记基线不触发、只对之后的新提交触发、commit-id 去重。
    #[tokio::test]
    async fn poll_records_baseline_then_triggers_only_on_new_commit() {
        let (pool, trigger, poll, probe) = poll_fixture(ScmType::Git, Some("main"), 5).await;
        let repo = TriggerRepo::new(pool.clone());
        let t0 = T0;
        let step = 5 * MIN; // 节奏 5 分钟

        // 首探：记基线 abc，不触发。
        probe.push_head(Some("abc"));
        let r = trigger.tick(t0).await.expect("首探");
        assert_eq!(r.poll_baseline, 1);
        assert_eq!(r.poll_fired, 0, "首探记基线不触发");
        assert!(builds(&pool).await.is_empty());
        assert_eq!(
            repo.get(poll.id)
                .await
                .unwrap()
                .unwrap()
                .baseline_commit
                .as_deref(),
            Some("abc")
        );

        // 同提交再探（一个节奏后）：去重，不触发。
        probe.push_head(Some("abc"));
        let r = trigger.tick(t0 + step).await.expect("去重");
        assert_eq!(r.poll_no_change, 1);
        assert!(builds(&pool).await.is_empty());

        // 新提交 def（再一个节奏后）：触发，基线更新。
        probe.push_head(Some("def"));
        let r = trigger.tick(t0 + 2 * step).await.expect("新提交");
        assert_eq!(r.poll_fired, 1, "新提交触发一次");
        assert_eq!(builds(&pool).await, vec![1]);
        assert_eq!(
            repo.get(poll.id)
                .await
                .unwrap()
                .unwrap()
                .baseline_commit
                .as_deref(),
            Some("def"),
            "基线更新到新提交"
        );

        // 触发上下文：poll 提交钉到 def。
        let build = BuildRepo::new(pool.clone())
            .get_by_number(1, "release", 1)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.trigger, TriggerSource::Poll);
        let detail: TriggerDetail = serde_json::from_str(&build.trigger_detail).expect("解析");
        assert_eq!(detail.by, "poll");
        assert_eq!(
            detail.commit.as_deref(),
            Some("def"),
            "poll 上下文钉轮询提交"
        );
    }

    /// AC：poll 按项目节奏轮询——未到期不探测。
    #[tokio::test]
    async fn poll_skips_before_interval() {
        let (pool, trigger, _poll, probe) = poll_fixture(ScmType::Git, Some("main"), 5).await;
        let t0 = T0;
        let step = 5 * MIN;
        // 首探记基线（t0）。
        probe.push_head(Some("abc"));
        trigger.tick(t0).await.expect("首探");
        // 新提交 ghi 排入，但距上次探测 < 5 分钟 → 未到期不探。
        probe.push_head(Some("ghi"));
        let r = trigger.tick(t0 + 4 * MIN).await.expect("未到期");
        assert_eq!(r.poll_fired, 0);
        assert_eq!(r.poll_no_change, 0, "未到期不探测");
        assert!(builds(&pool).await.is_empty());
        assert_eq!(probe.pending(), 1, "探测未消费");
        // 到期（+5 分钟）→ 探测 ghi → 触发。
        let r = trigger.tick(t0 + step).await.expect("到期");
        assert_eq!(r.poll_fired, 1);
        assert_eq!(builds(&pool).await, vec![1]);
        assert_eq!(probe.pending(), 0);
    }

    /// AC：poll 探测失败记入历史、按节奏重试、不自动禁用。
    #[tokio::test]
    async fn poll_probe_failure_records_history_and_retries_without_disabling() {
        let (pool, trigger, poll, probe) = poll_fixture(ScmType::Git, Some("main"), 5).await;
        let repo = TriggerRepo::new(pool.clone());
        let t0 = T0;
        let step = 5 * MIN;

        // 首探成功记基线。
        probe.push_head(Some("abc"));
        trigger.tick(t0).await.expect("首探");
        // 新提交 + 探测失败（一个节奏后）。
        probe.push_error("git ls-remote failed");
        let r = trigger.tick(t0 + step).await.expect("探测失败");
        assert_eq!(r.probe_errors, 1);
        assert!(builds(&pool).await.is_empty(), "失败不触发");
        let row = repo.get(poll.id).await.unwrap().unwrap();
        assert_eq!(
            row.last_probe_error.as_deref(),
            Some("git ls-remote failed")
        );
        assert!(row.enabled, "探测失败不自动禁用");
        assert_eq!(
            row.baseline_commit.as_deref(),
            Some("abc"),
            "基线不变 → 下次重试"
        );

        // 再探成功（同提交 abc，无新提交，再一个节奏后）→ 去重、错误清空。
        probe.push_head(Some("abc"));
        let r = trigger.tick(t0 + 2 * step).await.expect("再探成功");
        assert_eq!(r.poll_no_change, 1);
        let row = repo.get(poll.id).await.unwrap().unwrap();
        assert!(row.last_probe_error.is_none(), "成功清空历史错误");
    }

    /// AC：poll 空仓库（Ok(None)）不触发。
    #[tokio::test]
    async fn poll_empty_repo_does_not_trigger() {
        let (pool, trigger, _poll, probe) = poll_fixture(ScmType::Git, Some("main"), 5).await;
        probe.push_head(None);
        let r = trigger.tick(T0).await.expect("空仓库首探");
        // 空仓库首探：无 baseline 可记（无提交），归 no_commit。
        assert_eq!(r.poll_no_commit, 1);
        assert_eq!(r.poll_baseline, 0);
        assert!(builds(&pool).await.is_empty());
    }

    /// AC：启用 poll 触发器时重置基线 → 下次探测记当前 head 作基线、不触发
    /// （ADR-0016「启用时记基线不触发」：禁用期间落地的提交随基线吸收）。
    #[tokio::test]
    async fn poll_enable_resets_baseline_and_records_without_triggering() {
        let (pool, trigger, poll, probe) = poll_fixture(ScmType::Git, Some("main"), 5).await;
        let repo = TriggerRepo::new(pool.clone());
        let t0 = T0;
        let step = 5 * MIN;
        // 首探记基线 abc。
        probe.push_head(Some("abc"));
        trigger.tick(t0).await.expect("首探");
        // 模拟「禁用期间落地新提交 def，再启用」：reset_baseline 清基线。
        repo.reset_baseline(poll.id).await.expect("重置");
        // 下次探测见 baseline 缺失 → 记 def 作基线、不触发。
        probe.push_head(Some("def"));
        let r = trigger.tick(t0 + step).await.expect("启用后首探");
        assert_eq!(r.poll_baseline, 1, "启用后首探记基线、不触发");
        assert_eq!(r.poll_fired, 0);
        assert!(builds(&pool).await.is_empty());
        assert_eq!(
            repo.get(poll.id)
                .await
                .unwrap()
                .unwrap()
                .baseline_commit
                .as_deref(),
            Some("def")
        );
    }

    /// AC：svn 项目 poll 触发钉 revision（非 commit），无分支概念。
    #[tokio::test]
    async fn poll_svn_triggers_with_revision_not_commit() {
        let (pool, trigger, _poll, probe) = poll_fixture(ScmType::Svn, None, 5).await;
        let t0 = T0;
        let step = 5 * MIN;
        // 首探记基线 r100。
        probe.push_head(Some("r100"));
        trigger.tick(t0).await.expect("首探");
        // 新 revision r101（一个节奏后）→ 触发，钉 revision。
        probe.push_head(Some("r101"));
        let r = trigger.tick(t0 + step).await.expect("新 revision");
        assert_eq!(r.poll_fired, 1);
        let build = BuildRepo::new(pool.clone())
            .get_by_number(1, "release", 1)
            .await
            .expect("查")
            .expect("应存在");
        let detail: TriggerDetail = serde_json::from_str(&build.trigger_detail).expect("解析");
        assert_eq!(detail.commit, None, "svn 不钉 commit");
        assert_eq!(detail.revision.as_deref(), Some("r101"), "svn 钉 revision");
        assert_eq!(detail.branch, None, "svn 无分支概念");
    }

    /// AC：坏 spec 跳过（cron 非 5 字段、poll 节奏 < 1）——扫描不炸、记 spec_skipped。
    #[tokio::test]
    async fn bad_spec_is_skipped_without_aborting_scan() {
        let (_dir, pool, trigger) = fixture().await;
        save_project_and_pipeline(&pool, ScmType::Git, Some("main")).await;
        let repo = TriggerRepo::new(pool.clone());
        // 坏 cron：4 字段。
        repo.create(TriggerInput {
            project_id: 1,
            pipeline_name: "release".into(),
            kind: TriggerKind::Cron,
            spec: r#"{"expr":"0 2 * *"}"#.into(),
            enabled: true,
        })
        .await
        .expect("建坏 cron");
        // 坏 poll：节奏 0。
        repo.create(TriggerInput {
            project_id: 1,
            pipeline_name: "release".into(),
            kind: TriggerKind::Poll,
            spec: r#"{"interval_minutes":0}"#.into(),
            enabled: true,
        })
        .await
        .expect("建坏 poll");
        let report = trigger.tick(2 * 3600 * 1000).await.expect("扫描");
        assert_eq!(report.spec_skipped, 2, "两个坏 spec 都跳过");
        assert_eq!(report.cron_fired + report.poll_fired, 0);
    }

    // ---- spec 解析/校验 ----

    #[test]
    fn cron_spec_parse_validates_5_fields_and_cron_syntax() {
        assert!(CronSpec::parse(r#"{"expr":"0 2 * * *"}"#).is_ok());
        assert!(CronSpec::parse(r#"{"expr":"*/5 * * * *"}"#).is_ok());
        // 非 5 字段。
        assert!(
            CronSpec::parse(r#"{"expr":"0 2 * *"}"#).is_err(),
            "4 字段拒绝"
        );
        assert!(
            CronSpec::parse(r#"{"expr":"0 0 2 * * *"}"#).is_err(),
            "6 字段拒绝"
        );
        // 坏语法。
        assert!(
            CronSpec::parse(r#"{"expr":"99 2 * * *"}"#).is_err(),
            "坏分钟拒绝"
        );
        assert!(
            CronSpec::parse(r#"{"expr":"0 2 * * foo"}"#).is_err(),
            "坏字段拒绝"
        );
        // 坏 JSON。
        assert!(CronSpec::parse("not json").is_err());
    }

    #[test]
    fn poll_spec_parse_validates_interval_at_least_one_minute() {
        assert!(PollSpec::parse(r#"{"interval_minutes":5}"#).is_ok());
        assert!(PollSpec::parse(r#"{"interval_minutes":1}"#).is_ok());
        assert!(
            PollSpec::parse(r#"{"interval_minutes":0}"#).is_err(),
            "0 拒绝"
        );
        assert!(
            PollSpec::parse(r#"{"interval_minutes":-1}"#).is_err(),
            "负值拒绝"
        );
        assert!(PollSpec::parse("not json").is_err());
    }

    #[test]
    fn cron_to_json_round_trips_through_parse() {
        let spec = CronSpec {
            expr: "0 2 * * *".into(),
        };
        assert_eq!(CronSpec::parse(&spec.to_json()).unwrap(), spec);
    }

    #[test]
    fn poll_to_json_round_trips_through_parse() {
        let spec = PollSpec {
            interval_minutes: 7,
        };
        assert_eq!(PollSpec::parse(&spec.to_json()).unwrap(), spec);
    }

    // ---- 真实探测集成（本地裸仓库 fixture，AC1/AC2/AC6）----
    //
    // 用 SystemScmProbe（真实 git ls-remote）+ 本地裸仓库验证：poll 对新提交
    // 真实触发并创建构建全链路（AC1/AC6），探测失败走既有 trigger 逻辑记历史
    // 不禁用（AC2）。git 不可用时跳过（CI 自带 git）。

    use crate::scm::{ScmBins, SystemScmProbe};
    use crate::store::scm_credentials::ScmCredentialRepo;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command as StdCommand;

    /// git 是否可用（真实探测集成的前置；CI 自带 git，本地极少缺）。
    fn git_available() -> bool {
        StdCommand::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    /// 创建 src（非裸，main 一个提交）+ bare（src 的裸克隆），返回
    /// (src, bare, initial sha)。后续 [`advance_main`] 在 src 提交并 push 到 bare。
    fn real_bare_repo(parent: &Path) -> (PathBuf, PathBuf, String) {
        let src = parent.join("src");
        fs::create_dir_all(&src).expect("建 src");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}：{}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };
        git(&["init", "--quiet"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "test@sisyphus.local"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::write(src.join("hello.txt"), "v1\n").expect("写文件");
        git(&["add", "hello.txt"]);
        git(&["commit", "--quiet", "-m", "v1"]);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("utf8")
            .trim()
            .to_string();
        let bare = parent.join("bare");
        StdCommand::new("git")
            .args([
                "clone",
                "--bare",
                "--quiet",
                &src.to_string_lossy(),
                &bare.to_string_lossy(),
            ])
            .output()
            .expect("clone --bare");
        (src, bare, sha)
    }

    /// 在 src 提交 v2 并 push 到 bare（推进 bare 的 main head），返回新 sha。
    fn advance_main(src: &Path, bare: &Path) -> String {
        fs::write(src.join("hello.txt"), "v2\n").expect("改文件");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(src)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}：{}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };
        git(&["add", "hello.txt"]);
        git(&["commit", "--quiet", "-m", "v2"]);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("utf8")
            .trim()
            .to_string();
        // push src main → bare（本地裸仓库，免认证）。
        let push = StdCommand::new("git")
            .args(["push", "--quiet", &bare.to_string_lossy(), "main"])
            .current_dir(src)
            .output()
            .expect("push");
        assert!(
            push.status.success(),
            "push 失败：{}",
            String::from_utf8_lossy(&push.stderr)
        );
        sha
    }

    /// 真实探测 fixture：迁移库 + 项目 demo（git，scm_url 给定）+ release 定义 +
    /// poll 触发器 + SystemScmProbe（真实 git ls-remote）。返回 (pool, trigger, poll)。
    async fn real_poll_fixture(scm_url: String) -> (SqlitePool, TriggerEngine, TriggerRow) {
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
        ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url,
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        PipelineRepo::new(pool.clone())
            .save("demo", "release", &pipeline(), "tester")
            .await
            .expect("保存定义");
        let master_key = MasterKey::generate();
        let engine = Engine::new(pool.clone(), master_key, EventBus::new());
        let probe = Arc::new(SystemScmProbe::new(
            ScmCredentialRepo::new(pool.clone()),
            master_key,
            ScmBins::default(),
        )) as Arc<dyn ScmProbe>;
        let trigger = TriggerEngine::new(engine, pool.clone(), probe);
        let poll = TriggerRepo::new(pool.clone())
            .create(TriggerInput {
                project_id: 1,
                pipeline_name: "release".into(),
                kind: TriggerKind::Poll,
                spec: PollSpec {
                    interval_minutes: 5,
                }
                .to_json(),
                enabled: true,
            })
            .await
            .expect("建 poll");
        LEAK_DIR.lock().expect("leak slot").push(dir);
        (pool, trigger, poll)
    }

    /// AC1/AC6：真实探测下 poll 对新提交触发并创建构建（commit 钉到新提交）。
    #[tokio::test]
    async fn real_poll_triggers_on_new_commit_and_creates_build() {
        if !git_available() {
            eprintln!("skip: git 不可用");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let (src, bare, initial_sha) = real_bare_repo(dir.path());
        let (pool, trigger, poll) = real_poll_fixture(bare.to_string_lossy().into_owned()).await;
        let repo = TriggerRepo::new(pool.clone());
        let t0 = T0;
        let step = 5 * MIN;

        // 首探：记基线 initial_sha，不触发。
        let r = trigger.tick(t0).await.expect("首探");
        assert_eq!(r.poll_baseline, 1);
        assert!(builds(&pool).await.is_empty());
        assert_eq!(
            repo.get(poll.id)
                .await
                .unwrap()
                .unwrap()
                .baseline_commit
                .as_deref(),
            Some(initial_sha.as_str())
        );

        // 新提交：推进 bare 的 main。
        let new_sha = advance_main(&src, &bare);
        assert_ne!(new_sha, initial_sha);

        // 到期再探 → 新提交 → 触发构建，commit 钉到新提交、基线更新。
        let r = trigger.tick(t0 + step).await.expect("新提交");
        assert_eq!(r.poll_fired, 1, "新提交触发一次");
        assert_eq!(builds(&pool).await, vec![1]);
        let build = BuildRepo::new(pool.clone())
            .get_by_number(1, "release", 1)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.trigger, TriggerSource::Poll);
        let detail: TriggerDetail = serde_json::from_str(&build.trigger_detail).expect("解析");
        assert_eq!(
            detail.commit.as_deref(),
            Some(new_sha.as_str()),
            "poll 上下文钉轮询提交"
        );
        assert_eq!(
            repo.get(poll.id)
                .await
                .unwrap()
                .unwrap()
                .baseline_commit
                .as_deref(),
            Some(new_sha.as_str()),
            "基线更新到新提交"
        );
        let _ = dir;
    }

    /// AC2：真实探测失败走既有 trigger 逻辑——记 last_probe_error、按节奏重试、
    /// 不自动禁用（基线不动）。
    #[tokio::test]
    async fn real_poll_probe_failure_records_history_without_disabling() {
        if !git_available() {
            eprintln!("skip: git 不可用");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let bad_url = dir
            .path()
            .join("no-such-repo")
            .to_string_lossy()
            .into_owned();
        let (pool, trigger, poll) = real_poll_fixture(bad_url).await;
        let repo = TriggerRepo::new(pool.clone());

        let r = trigger.tick(T0).await.expect("探测失败");
        assert_eq!(r.probe_errors, 1, "探测失败记 probe_errors");
        assert!(builds(&pool).await.is_empty(), "失败不触发");
        let row = repo.get(poll.id).await.unwrap().unwrap();
        assert!(
            row.last_probe_error.is_some(),
            "记 last_probe_error：{:?}",
            row.last_probe_error
        );
        assert!(row.enabled, "探测失败不自动禁用");
        assert!(
            row.baseline_commit.is_none(),
            "失败不记基线 → 下次节奏对新提交重试"
        );
        let _ = dir;
    }
}

//! engine 模块：构建编排状态机（票 #46，ADR-0006，消费 #45 的数据底座）。
//!
//! 职责（ADR-0009）：
//! - **统一触发入口 [`Engine::start_build`]**：手动/cron/poll 三来源各自产出
//!   触发参数（[`TriggerDetail`]）→ 取当前定义 + revision 组装 `BuildSnapshot`
//!   → 落 builds 行（queued）→ 事件广播。构建号与快照在入队时即定，定义
//!   保存不影响已入队/运行中构建（ADR-0006）。
//! - **构建推进 [`Engine::drive`]**：排队→运行（FIFO 放行：同 pipeline 同时
//!   只跑一条，后来者排队）→ 阶段按序 → 阶段级 when 求值（不满足整阶段
//!   跳过、其内任务全不发）→ 阶段内任务并行下发 → 任务终态收集 → 全部
//!   成功进下一阶段 → 终态（succeeded / failed）。
//! - **任务终态 [`Engine::on_job_terminal`]**（sched/grpc 上报接线）：成功 →
//!   推进；失败且非 allow_failure → 自动重试（retry_count 未耗尽 → 同 job
//!   新 attempt 重新入池）或 fail-fast 级联（同阶段未完成任务 cancelled、
//!   后续阶段 skipped、构建 failed）；allow_failure 豁免。
//! - **ResolvedJobSpec 组装**：见 [`spec`] 模块——变量替换（`SISY_WORKSPACE`
//!   占位保留）、env 合并、机密按名解密注入、隐式容器标签、只含待跑节点，
//!   组装产物落 jobs.spec_json 快照。任务引用不存在的机密名 → 下发前立即
//!   失败（detail 记名）走 fail-fast（ADR-0015）。
//! - **重跑**：从头重跑（新号 attempt=1）经 `start_build`；从失败任务重跑
//!   （同号 attempt+1、成功任务保留）经 store `rerun_from_failed` 后由 drive
//!   按 build.attempt 重开未成功任务（失败/取消/跳过者），成功者保留。
//!
//! 本票任务终态由测试注入（store 缝/内联驱动），不真下发 Agent——sched
//! 与 grpc 批次在 [`Engine::on_job_terminal`] 上接线。

mod spec;

pub(crate) use spec::{AssembleError, ResolvedJobSpec, ResolvedStep, Vcs, eval_when, var_env};

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sisyphus_model::pipeline::{Job, Pipeline, Revision, Stage};
use sisyphus_model::validate::BuildSnapshot;
use sqlx::SqlitePool;

use crate::events::{Event, EventBus};
use crate::secrets::MasterKey;
use crate::store::builds::{BuildRepo, BuildRow, BuildStatus, StartBuild, TriggerSource};
use crate::store::jobs::{JobRepo, JobRow, JobStatus, NewJob};
use crate::store::pipelines::PipelineRepo;
use crate::store::projects::{Project, ProjectRepo};
use crate::store::secrets::SecretRepo;
use crate::store::{StoreError, now_ms};

/// 触发上下文（builds.trigger_detail 的 JSON 形态）。三来源各自产出：
/// 手动 = 触发人 + 可选分支/commit/revision + 参数覆盖；cron = 默认值 +
/// 默认分支 head；poll = 轮询到的提交。参数覆盖以「名 + 字符串值」呈现
/// （与 `${}` 插值同型；类型转换由触发端点按参数定义校验）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerDetail {
    /// 触发人（业务表实名；审计不双重记账，ADR-0015）。
    pub by: String,
    /// git 分支（手动可选；缺省项目默认分支；svn 无分支概念为空）。
    pub branch: Option<String>,
    /// git commit sha（手动未钉为空；poll 为轮询到的提交）。
    pub commit: Option<String>,
    /// svn revision（手动可选；git 为空）。
    pub revision: Option<String>,
    /// 参数覆盖（手动触发；默认值之上叠加）。
    pub params: Vec<ParameterOverride>,
}

/// 参数覆盖（「默认值，手动触发可覆盖」，ADR-0006）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterOverride {
    /// 参数名。
    pub name: String,
    /// 覆盖值（字符串形式，与 `${}` 插值同型）。
    pub value: String,
}

/// `start_build` 统一入口的输入。
#[derive(Debug, Clone)]
pub struct StartBuildInput {
    /// 项目名（API 寻径键；行内以 project_id 落库）。
    pub project_name: String,
    /// pipeline 名。
    pub pipeline_name: String,
    /// 触发源（manual/cron/poll）。
    pub trigger: TriggerSource,
    /// 触发上下文（三来源各自产出）。
    pub detail: TriggerDetail,
}

/// 构建编排状态机（组合根装配：pool → repo → engine，API/sched/grpc 共享）。
#[derive(Debug, Clone)]
pub struct Engine {
    builds: BuildRepo,
    jobs: JobRepo,
    pipelines: PipelineRepo,
    projects: ProjectRepo,
    secrets: SecretRepo,
    master_key: MasterKey,
    bus: EventBus,
}

impl Engine {
    /// 由连接池装配（与 `AppState` 同组合根形态）。
    pub fn new(pool: SqlitePool, master_key: MasterKey, bus: EventBus) -> Self {
        Self {
            builds: BuildRepo::new(pool.clone()),
            jobs: JobRepo::new(pool.clone()),
            pipelines: PipelineRepo::new(pool.clone()),
            projects: ProjectRepo::new(pool.clone()),
            secrets: SecretRepo::new(pool.clone()),
            master_key,
            bus,
        }
    }

    /// 事件总线引用（sched/grpc 订阅热通知）。
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    // -----------------------------------------------------------------------
    // 统一触发入口
    // -----------------------------------------------------------------------

    /// 手动/cron/poll 三来源的统一入口：取当前定义 + revision 组装快照 →
    /// 落 queued 构建行（per-pipeline 并发单调构建号）→ 广播 BuildCreated。
    /// 快照语义：构建号与快照在入队时即定，后续定义保存不影响本构建
    /// （ADR-0006）。排队推进由 [`Self::drive`]（FIFO 放行）。
    pub async fn start_build(&self, input: StartBuildInput) -> Result<BuildRow, StoreError> {
        let project = self
            .projects
            .get_by_name(&input.project_name)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("项目 {} 不存在", input.project_name)))?;
        let stored = self
            .pipelines
            .get(&input.project_name, &input.pipeline_name)
            .await?
            .ok_or_else(|| {
                StoreError::NotFound(format!("pipeline {} 不存在", input.pipeline_name))
            })?;
        let pipeline: Pipeline =
            serde_json::from_str(&stored.definition).map_err(StoreError::DefinitionJson)?;
        let snapshot = BuildSnapshot::new(
            pipeline,
            Revision {
                number: stored.revision,
                operator: stored.operator,
                at_ms: stored.updated_at,
            },
        );
        let trigger_detail =
            serde_json::to_string(&input.detail).map_err(StoreError::DefinitionJson)?;
        let row = self
            .builds
            .start(StartBuild {
                project_id: project.id,
                pipeline_name: input.pipeline_name,
                trigger: input.trigger,
                trigger_detail,
                snapshot,
            })
            .await?;
        self.bus.publish(Event::BuildCreated {
            build_id: row.id,
            project_name: project.name,
            pipeline_name: row.pipeline_name.clone(),
            number: row.number,
            trigger: row.trigger,
        });
        Ok(row)
    }

    // -----------------------------------------------------------------------
    // 构建推进
    // -----------------------------------------------------------------------

    /// 推进一次构建：排队→运行（FIFO 放行，原子裁决——同 pipeline 无运行中
    /// 构建才提升最老排队者，可能不是本构建）→ 阶段推进。sched 循环与测试
    /// 共用此入口。
    pub async fn drive(&self, build_id: i64) -> Result<(), StoreError> {
        let Some(build) = self.builds.get(build_id).await? else {
            return Ok(());
        };
        let promoted = if build.status == BuildStatus::Queued {
            self.builds
                .promote_oldest_if_idle(build.project_id, &build.pipeline_name, now_ms())
                .await?
        } else {
            None
        };
        let build = match promoted {
            Some(promoted) => {
                let project_name = self.project_name(promoted.project_id).await?;
                self.publish_build_status(&promoted, &project_name);
                promoted
            }
            None => build,
        };
        if build.status != BuildStatus::Running {
            return Ok(());
        }
        self.drive_running(build).await
    }

    /// 项目名（事件广播与 detail 用；构建属主缺失时为空串——事件只是热
    /// 通知，构建号/状态为主消费面）。
    async fn project_name(&self, project_id: i64) -> Result<String, StoreError> {
        Ok(self
            .projects
            .get_by_id(project_id)
            .await?
            .map(|p| p.name)
            .unwrap_or_default())
    }

    /// 运行中构建的阶段推进（单次调用可推进多个「无行/全终态」阶段，遇
    /// 在跑阶段停下等任务终态）。任务终态由 [`Self::on_job_terminal`] 驱动
    /// 再次进入。
    async fn drive_running(&self, build: BuildRow) -> Result<(), StoreError> {
        let now = now_ms();
        let snapshot = match parse_snapshot(&build) {
            Ok(snapshot) => snapshot,
            Err(e) => return self.fail_build_defensively(&build, &e.to_string()).await,
        };
        let Some(project) = self.projects.get_by_id(build.project_id).await? else {
            return self
                .fail_build_defensively(&build, "构建属主项目缺失")
                .await;
        };
        let trigger: TriggerDetail = match serde_json::from_str(&build.trigger_detail) {
            Ok(t) => t,
            Err(e) => {
                return self
                    .fail_build_defensively(&build, &format!("触发上下文损坏：{e}"))
                    .await;
            }
        };
        let params = merged_params(&snapshot, &trigger);
        let scm = spec::scm_context(&project, &trigger);
        let project_name = project.name.clone();
        let ctx = DriveCtx {
            build: &build,
            snapshot: &snapshot,
            project: &project,
            trigger: &trigger,
            params: &params,
            scm: &scm,
            project_name: &project_name,
        };

        if snapshot.pipeline.stages.is_empty() {
            // 无阶段 pipeline：入队即成功（与全阶段终态裁决同路径）。
            return self
                .finish_build(&build, &project_name, BuildStatus::Succeeded)
                .await;
        }

        loop {
            // 级联可能已把构建置 failed（如任务下发时缺机密）——检查后停止。
            if let Some(fresh) = self.builds.get(build.id).await?
                && fresh.status != BuildStatus::Running
            {
                return Ok(());
            }
            let jobs = self.jobs.list_by_build(build.id).await?;

            // 当前阶段 = 第一个「无行 / 含非终态任务 / 全终态但待重跑续开」
            // 的阶段（阶段按序；重跑时 build.attempt 已 +1，全终态行的最大
            // attempt 小于它即需重开失败任务）。
            let current = snapshot
                .pipeline
                .stages
                .iter()
                .enumerate()
                .find_map(|(i, _stage)| {
                    let rows: Vec<&JobRow> =
                        jobs.iter().filter(|j| j.stage_index == i as i32).collect();
                    if rows.is_empty()
                        || rows.iter().any(|r| !r.status.is_terminal())
                        || (rows.iter().all(|r| r.status.is_terminal())
                            && rows.iter().map(|r| r.attempt).max().unwrap_or(0) < build.attempt)
                    {
                        Some(i)
                    } else {
                        None
                    }
                });

            let Some(stage_index) = current else {
                // 全部阶段已终态 → 构建终态裁决。成败只看每任务的最新 attempt
                // （重跑后旧失败行是历史，ADR-0006 重跑语义）；allow_failure
                // 豁免的失败不计入。
                let latest = latest_per_job(&jobs);
                let failed_stage = latest
                    .iter()
                    .filter(|j| !j.allow_failure && j.status.is_failure())
                    .map(|j| j.stage_index)
                    .min();
                if let Some(stage) = failed_stage {
                    // 兜底：正常路径级联已置 failed；此处自愈（级联幂等）。
                    self.builds.fail_fast_cascade(build.id, stage, now).await?;
                } else {
                    self.builds
                        .transition(build.id, BuildStatus::Succeeded, now)
                        .await?;
                }
                return self.finish_build_event(&build, &project_name).await;
            };

            let stage = &snapshot.pipeline.stages[stage_index];
            let rows: Vec<JobRow> = jobs
                .into_iter()
                .filter(|j| j.stage_index == stage_index as i32)
                .collect();

            if rows.is_empty() {
                // 阶段从未裁决：阶段级 when（不满足 → 整阶段跳过，其内任务
                // 全不发——落 Skipped 行不留任何可跑任务）；满足 → 全部任务
                // 组装 + 下发（attempt = build.attempt）。
                let pass = stage.when.as_deref().is_none_or(|w| {
                    let env = var_env(ctx.build, ctx.project, stage, None, &scm, ctx.params);
                    eval_when(w, &env, "", &stage.name)
                });
                if pass {
                    for job_def in &stage.jobs {
                        self.spawn_job(&ctx, stage_index, stage, job_def, now)
                            .await?;
                    }
                } else {
                    for job_def in &stage.jobs {
                        let row = self.insert_job(&build, stage_index, job_def, None).await?;
                        self.jobs
                            .transition(row.id, JobStatus::Skipped, None, None, now)
                            .await?;
                        let updated = self.jobs.get(row.id).await?.expect("刚迁移的行必存在");
                        self.publish_job_status(&updated);
                    }
                }
                continue; // 已裁决（下发或跳过），下一轮推进下一阶段。
            }

            if rows.iter().all(|r| r.status.is_terminal()) {
                // 全终态：正常完成（build.attempt == 行内最大 attempt）或
                // 从失败任务重跑待续（build.attempt 已 +1）。
                let max_attempt = rows.iter().map(|r| r.attempt).max().unwrap_or(0);
                if build.attempt > max_attempt {
                    // 重跑：成功任务保留，其余（失败/取消/跳过）按新 attempt
                    // 重新下发，失败任务起继续（ADR-0006 重跑语义）。阶段级
                    // when 重新求值——重跑仍处「构建 #N 当时」语义，之前被
                    // 跳过的阶段（when 不满足）其任务依然不发。
                    let stage_pass = stage.when.as_deref().is_none_or(|w| {
                        let env = var_env(ctx.build, ctx.project, stage, None, ctx.scm, ctx.params);
                        eval_when(w, &env, "", &stage.name)
                    });
                    if !stage_pass {
                        continue; // 阶段仍不满足：保持跳过，下一阶段。
                    }
                    for job_def in &stage.jobs {
                        let latest = rows
                            .iter()
                            .filter(|r| r.name == job_def.name)
                            .max_by_key(|r| r.attempt);
                        if latest.is_some_and(|r| r.status == JobStatus::Succeeded) {
                            continue;
                        }
                        self.spawn_job(&ctx, stage_index, stage, job_def, now)
                            .await?;
                    }
                    continue;
                }
                continue; // 正常完成 → 下一阶段。
            }

            // 阶段在跑：任务级 when 求值 + 补组装（spec 缺失者）。
            for job_row in rows.iter().filter(|r| r.status == JobStatus::Queued) {
                let Some(job_def) = stage.jobs.iter().find(|j| j.name == job_row.name) else {
                    // 行与定义失配（快照损坏级）：判失败走级联。
                    tracing::error!(
                        build_id = build.id,
                        stage_index,
                        job = %job_row.name,
                        "任务行与快照定义失配"
                    );
                    self.fail_job_and_cascade(
                        &build,
                        &project_name,
                        job_row,
                        "任务定义与快照失配",
                        now,
                    )
                    .await?;
                    return Ok(());
                };
                let env = var_env(
                    ctx.build,
                    ctx.project,
                    stage,
                    Some(job_def),
                    &scm,
                    ctx.params,
                );
                if job_def
                    .when
                    .as_deref()
                    .is_some_and(|w| !eval_when(w, &env, &job_def.name, &stage.name))
                {
                    self.jobs
                        .transition(job_row.id, JobStatus::Skipped, None, None, now)
                        .await?;
                    let updated = self.jobs.get(job_row.id).await?.expect("刚迁移的行必存在");
                    self.publish_job_status(&updated);
                    continue;
                }
                if job_row.spec_json.is_none() {
                    match self.assemble_spec(&ctx, stage_index, stage, job_def).await {
                        Ok(Ok(spec)) => {
                            let spec_json =
                                serde_json::to_string(&spec).map_err(StoreError::DefinitionJson)?;
                            self.jobs.set_spec(job_row.id, &spec_json).await?;
                        }
                        Ok(Err(err)) => {
                            self.fail_job_and_cascade(
                                &build,
                                &project_name,
                                job_row,
                                &err.to_string(),
                                now,
                            )
                            .await?;
                            return Ok(());
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            return Ok(()); // 阶段在跑：等任务终态再推进。
        }
    }

    /// 下发一个任务：组装（机密解密注入；缺失/解密失败 → 立即失败走级联）
    /// → 落 queued 行（spec 快照随行，attempt = build.attempt）。
    async fn spawn_job(
        &self,
        ctx: &DriveCtx<'_>,
        stage_index: usize,
        stage: &Stage,
        job_def: &Job,
        now: i64,
    ) -> Result<(), StoreError> {
        match self.assemble_spec(ctx, stage_index, stage, job_def).await {
            Ok(Ok(spec)) => {
                let row = self
                    .insert_job(ctx.build, stage_index, job_def, Some(spec))
                    .await?;
                self.publish_job_status(&row);
                Ok(())
            }
            Ok(Err(err)) => {
                // 机密缺失/解密失败：任务立即失败（detail 记名）并级联 fail-fast。
                let row = self
                    .insert_job(ctx.build, stage_index, job_def, None)
                    .await?;
                let fresh = self.jobs.get(row.id).await?.expect("刚插入的行必存在");
                self.fail_job_and_cascade(
                    ctx.build,
                    ctx.project_name,
                    &fresh,
                    &err.to_string(),
                    now,
                )
                .await?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 落一行任务（queued；spec 可空——Skipped/失败行无 spec，调用侧再迁移）。
    async fn insert_job(
        &self,
        build: &BuildRow,
        stage_index: usize,
        job_def: &Job,
        spec: Option<ResolvedJobSpec>,
    ) -> Result<JobRow, StoreError> {
        let spec_json = spec
            .as_ref()
            .map(|s| serde_json::to_string(s).map_err(StoreError::DefinitionJson))
            .transpose()?;
        let labels = spec.map(|s| s.labels).unwrap_or_default();
        self.jobs
            .insert(NewJob {
                build_id: build.id,
                stage_index: stage_index as i32,
                name: job_def.name.clone(),
                attempt: build.attempt,
                spec_json,
                agent_id: None,
                labels,
                timeout_minutes: job_def.timeout_minutes as i32,
                retry_count: job_def.retry_count as i32,
                allow_failure: job_def.allow_failure,
            })
            .await
    }

    /// 组装一份 ResolvedJobSpec：外层 `Err` = 存储读失败（系统级，上抛）；
    /// 内层 [`AssembleError`] = 组装失败（任务级——机密缺失/解密失败）。
    async fn assemble_spec(
        &self,
        ctx: &DriveCtx<'_>,
        stage_index: usize,
        stage: &Stage,
        job_def: &Job,
    ) -> Result<Result<ResolvedJobSpec, AssembleError>, StoreError> {
        let names: Vec<&str> = job_def.secrets.iter().map(String::as_str).collect();
        let secret_ciphertexts = self.secrets.ciphertexts(ctx.project.id, &names).await?;
        Ok(spec::assemble(&spec::AssembleInput {
            build: ctx.build,
            snapshot: ctx.snapshot,
            stage_index,
            stage,
            job: job_def,
            project: ctx.project,
            trigger: ctx.trigger,
            params: ctx.params,
            secret_ciphertexts: &secret_ciphertexts,
            master_key: &self.master_key,
        }))
    }

    /// 任务立即失败（detail 记名）并级联 fail-fast：同阶段未完成任务
    /// cancelled、后续阶段 skipped、构建 failed（ADR-0006）。
    async fn fail_job_and_cascade(
        &self,
        build: &BuildRow,
        project_name: &str,
        job: &JobRow,
        detail: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        if job.status != JobStatus::Failed {
            self.jobs
                .transition(job.id, JobStatus::Failed, None, Some(detail), now)
                .await?;
        }
        let failed = self.jobs.get(job.id).await?.expect("行必存在");
        self.publish_job_status(&failed);
        self.builds
            .fail_fast_cascade(build.id, job.stage_index, now)
            .await?;
        let failed_build = self.builds.get(build.id).await?.expect("构建必存在");
        self.publish_build_status(&failed_build, project_name);
        Ok(())
    }

    /// 防御性失败（快照/属主/触发上下文损坏）：构建置 failed 并广播。
    async fn fail_build_defensively(
        &self,
        build: &BuildRow,
        reason: &str,
    ) -> Result<(), StoreError> {
        tracing::error!(build_id = build.id, "构建数据损坏：{reason}");
        self.builds
            .transition(build.id, BuildStatus::Failed, now_ms())
            .await?;
        let failed = self.builds.get(build.id).await?.expect("构建必存在");
        let project_name = self.project_name(build.project_id).await?;
        self.publish_build_status(&failed, &project_name);
        Ok(())
    }

    /// 构建置终态并广播（读回新鲜行——内存行状态字段已过期）。
    async fn finish_build(
        &self,
        build: &BuildRow,
        project_name: &str,
        status: BuildStatus,
    ) -> Result<(), StoreError> {
        self.builds.transition(build.id, status, now_ms()).await?;
        self.finish_build_event(build, project_name).await
    }

    /// 广播构建当前状态（读回新鲜行）。
    async fn finish_build_event(
        &self,
        build: &BuildRow,
        project_name: &str,
    ) -> Result<(), StoreError> {
        let fresh = self.builds.get(build.id).await?.expect("构建必存在");
        self.publish_build_status(&fresh, project_name);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 任务终态（sched/grpc 上报接线点）
    // -----------------------------------------------------------------------

    /// 任务终态上报处理（本票由测试注入任务结果驱动；sched 批次接 Agent
    /// 上报）。语义（ADR-0006）：
    /// - 成功（或 allow_failure 豁免的失败）→ 计数推进（drive 检查阶段是否
    ///   全部终态）。
    /// - 失败且非 allow_failure → retry_count 未耗尽 → 同 job 新 attempt
    ///   重新入池；耗尽 → fail-fast 级联。
    /// - timeout/aborted（同失败类，ADR-0008）→ 同上。
    pub async fn on_job_terminal(&self, job: &JobRow, now: i64) -> Result<(), StoreError> {
        match job.status {
            JobStatus::Succeeded => self.drive(job.build_id).await,
            JobStatus::Failed if job.allow_failure => self.drive(job.build_id).await,
            // 失败类终态（failed/timeout/aborted，ADR-0008 同失败面）。
            JobStatus::Failed | JobStatus::Timeout | JobStatus::Aborted => {
                // 自动重试：attempt 未耗尽 → 同 job 新 attempt 重新入池。
                if job.attempt <= job.retry_count
                    && let Some(next) = self.jobs.next_attempt(job.id).await?
                {
                    self.publish_job_status(&next);
                    return Ok(());
                }
                let Some(build) = self.builds.get(job.build_id).await? else {
                    return Ok(());
                };
                let project_name = self.project_name(build.project_id).await?;
                self.fail_job_and_cascade(&build, &project_name, job, "任务失败", now)
                    .await
            }
            // cancelled/skipped 由级联/跳过路径直接落库，不经本入口；
            // queued/running/unknown 非终态，不是本入口的职责。
            JobStatus::Queued
            | JobStatus::Running
            | JobStatus::Unknown
            | JobStatus::Cancelled
            | JobStatus::Skipped => Ok(()),
        }
    }

    // -----------------------------------------------------------------------
    // 构建取消（票 B2c-T5 REST 面入口）
    // -----------------------------------------------------------------------

    /// 取消构建（build 级）：排队中移出 pending 池、构建置 cancelled、发布
    /// `BuildStatus{Cancelled}` 事件——sched 循环订阅后经通道向在途
    /// running/unknown 任务下发 CancelBuild（与 fail-fast 级联同款事件路径：
    /// engine 做 DB 状态迁移 + 发事件，sched 据事件下发取消）。终态幂等
    /// （已终态构建原样返回，不重复迁移）。构建不存在返回 `None`（REST 映射
    /// 404）。离线 Agent 的在途任务取消挂起重连补发（DB 可重建：
    /// `channel_cancel_pending` 视图）。
    pub async fn cancel_build(&self, build_id: i64) -> Result<Option<BuildRow>, StoreError> {
        let now = now_ms();
        let Some(build) = self.builds.get(build_id).await? else {
            return Ok(None);
        };
        if build.status.is_terminal() {
            return Ok(Some(build));
        }
        self.builds
            .transition(build_id, BuildStatus::Cancelled, now)
            .await?;
        self.jobs.cancel_queued_by_build(build_id, now).await?;
        let cancelled = self
            .builds
            .get(build_id)
            .await?
            .expect("刚迁移的构建必存在");
        let project_name = self.project_name(build.project_id).await?;
        self.publish_build_status(&cancelled, &project_name);
        Ok(Some(cancelled))
    }

    // -----------------------------------------------------------------------
    // 事件广播
    // -----------------------------------------------------------------------

    fn publish_build_status(&self, build: &BuildRow, project_name: &str) {
        self.bus.publish(Event::BuildStatus {
            build_id: build.id,
            project_name: project_name.to_string(),
            pipeline_name: build.pipeline_name.clone(),
            number: build.number,
            status: build.status,
            attempt: build.attempt,
        });
    }

    fn publish_job_status(&self, job: &JobRow) {
        self.bus.publish(Event::JobStatus {
            job_id: job.id,
            build_id: job.build_id,
            stage_index: job.stage_index,
            name: job.name.clone(),
            status: job.status,
            attempt: job.attempt,
        });
    }
}

/// 阶段组装上下文（drive 循环内的借用集合：构建/快照/项目/触发/参数/SCM，
/// 收敛 spawn/assemble 与 when 求值的长参数列表）。
struct DriveCtx<'a> {
    build: &'a BuildRow,
    snapshot: &'a BuildSnapshot,
    project: &'a Project,
    trigger: &'a TriggerDetail,
    params: &'a HashMap<String, String>,
    scm: &'a spec::ScmContext,
    project_name: &'a str,
}

/// 解析构建快照（BuildSnapshot JSON；schema 不解析内部，此处是收敛点）。
fn parse_snapshot(build: &BuildRow) -> Result<BuildSnapshot, StoreError> {
    serde_json::from_str(&build.snapshot).map_err(StoreError::DefinitionJson)
}

/// 参数值合并：默认值打底、手动覆盖叠加（ADR-0006「默认值，手动触发可
/// 覆盖」；不存在的覆盖名无害——未被引用即无效果）。
fn merged_params(snapshot: &BuildSnapshot, detail: &TriggerDetail) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for p in &snapshot.pipeline.parameters {
        if let Some(default) = &p.default {
            params.insert(p.name.clone(), default.as_str());
        }
    }
    for o in &detail.params {
        params.insert(o.name.clone(), o.value.clone());
    }
    params
}

/// 每任务的最新 attempt 行（终态裁决与重跑语义共用：同 (stage, name) 多行
/// 时取 attempt 最大者，旧 attempt 的失败是历史、不参与成败判定）。
fn latest_per_job(jobs: &[JobRow]) -> Vec<&JobRow> {
    let mut by_key: HashMap<(i32, &str), &JobRow> = HashMap::new();
    for job in jobs {
        let key = (job.stage_index, job.name.as_str());
        let replace = match by_key.get(&key) {
            Some(existing) => job.attempt > existing.attempt,
            None => true,
        };
        if replace {
            by_key.insert(key, job);
        }
    }
    by_key.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::{NewProject, ScmType};
    use sisyphus_model::pipeline::{
        EnvVar, ExecutionEnv, Parameter, ParameterType, ParameterValue, Shell, Step,
    };

    /// 独立临时目录 + 已迁移库 + 项目 demo（engine 缝测试形态）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool, Engine) {
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
                scm_url: "https://example.com/repo".into(),
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        let engine = Engine::new(pool.clone(), MasterKey::generate(), EventBus::new());
        (dir, pool, engine)
    }

    fn trigger(branch: Option<&str>) -> TriggerDetail {
        TriggerDetail {
            by: "alice".into(),
            branch: branch.map(str::to_string),
            commit: None,
            revision: None,
            params: vec![],
        }
    }

    /// 三阶段 pipeline：阶段 0 两个任务（compile 无 when、lint 任务级 when
    /// main 分支）、阶段 1 一个容器任务 unit（retry_count=1）、阶段 2 一个
    /// 任务（阶段级 when prod 分支 → main 触发整阶段跳过）。
    fn pipeline() -> Pipeline {
        Pipeline {
            name: "release".into(),
            parameters: vec![Parameter {
                name: "target".into(),
                r#type: ParameterType::String,
                required: true,
                default: Some(ParameterValue::String("x86_64".into())),
                description: None,
                choices: vec![],
            }],
            env: vec![EnvVar {
                name: "CARGO_HOME".into(),
                value: "${SISY_WORKSPACE}/.cargo".into(),
            }],
            notification: None,
            stages: vec![
                Stage {
                    name: "build".into(),
                    when: None,
                    jobs: vec![
                        Job {
                            name: "compile".into(),
                            exec_env: None,
                            labels: vec!["sisyphus/os=linux".into()],
                            when: None,
                            env: vec![EnvVar {
                                name: "MODE".into(),
                                value: "${target}".into(),
                            }],
                            allow_failure: false,
                            retry_count: 0,
                            timeout_minutes: 30,
                            artifact_uploads: vec![],
                            artifact_downloads: vec![],
                            caches: vec![],
                            secrets: vec![],
                            steps: vec![Step::Shell {
                                command: "echo ${SISY_BUILD_NUMBER} ${target}".into(),
                                shell: Some(Shell::Bash),
                                when: None,
                            }],
                        },
                        Job {
                            name: "lint".into(),
                            exec_env: None,
                            labels: vec![],
                            when: Some("${SISY_BRANCH} == \"main\"".into()),
                            env: vec![],
                            allow_failure: false,
                            retry_count: 0,
                            timeout_minutes: 0,
                            artifact_uploads: vec![],
                            artifact_downloads: vec![],
                            caches: vec![],
                            secrets: vec![],
                            steps: vec![Step::Shell {
                                command: "echo lint".into(),
                                shell: None,
                                when: None,
                            }],
                        },
                    ],
                },
                Stage {
                    name: "test".into(),
                    when: None,
                    jobs: vec![Job {
                        name: "unit".into(),
                        exec_env: Some(ExecutionEnv::Container {
                            image: "rust:1.97".into(),
                        }),
                        labels: vec![],
                        when: None,
                        env: vec![],
                        allow_failure: false,
                        retry_count: 1,
                        timeout_minutes: 0,
                        artifact_uploads: vec![],
                        artifact_downloads: vec![],
                        caches: vec![],
                        secrets: vec![],
                        steps: vec![Step::Shell {
                            command: "cargo test".into(),
                            shell: None,
                            when: None,
                        }],
                    }],
                },
                Stage {
                    name: "deploy".into(),
                    when: Some("${SISY_BRANCH} == \"prod\"".into()),
                    jobs: vec![Job {
                        name: "publish".into(),
                        exec_env: None,
                        labels: vec![],
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
                            command: "publish".into(),
                            shell: None,
                            when: None,
                        }],
                    }],
                },
            ],
            revision: None,
        }
    }

    async fn save_and_start(engine: &Engine, pipeline: Pipeline, branch: Option<&str>) -> BuildRow {
        engine
            .pipelines
            .save("demo", "release", &pipeline, "tester")
            .await
            .expect("保存定义");
        engine
            .start_build(StartBuildInput {
                project_name: "demo".into(),
                pipeline_name: "release".into(),
                trigger: TriggerSource::Manual,
                detail: trigger(branch),
            })
            .await
            .expect("触发构建")
    }

    fn job_by_name<'a>(jobs: &'a [JobRow], name: &str) -> &'a JobRow {
        jobs.iter().find(|j| j.name == name).expect("任务应存在")
    }

    /// 任务名的最新 attempt 行（重试/重跑后同任务多行：attempt 最大者）。
    fn latest_job_by_name<'a>(jobs: &'a [JobRow], name: &str) -> &'a JobRow {
        jobs.iter()
            .filter(|j| j.name == name)
            .max_by_key(|j| j.attempt)
            .expect("任务应存在")
    }

    /// 任务终态注入（本票测试缝）：先把任务行迁移到目标终态（sched/grpc
    /// 上报路径的落库动作），再以终态行调 `on_job_terminal` 驱动推进。
    async fn report_job(engine: &Engine, job: &JobRow, status: JobStatus) {
        let exit_code = if status == JobStatus::Succeeded {
            Some(0)
        } else {
            Some(1)
        };
        engine
            .jobs
            .transition(job.id, status, exit_code, None, 1_000)
            .await
            .expect("任务迁移");
        let updated = engine.jobs.get(job.id).await.expect("查").expect("应存在");
        engine
            .on_job_terminal(&updated, 1_000)
            .await
            .expect("上报终态");
    }

    /// 驱动构建到终态（测试注入：把所有非终态任务按成功上报，直至构建
    /// 终态）。阶段下发是增量的，需多轮。
    async fn complete_build_ok(engine: &Engine, build_id: i64) {
        loop {
            let build = engine
                .builds
                .get(build_id)
                .await
                .expect("查")
                .expect("应存在");
            if build.status.is_terminal() {
                return;
            }
            let jobs = engine.jobs.list_by_build(build_id).await.expect("任务清单");
            for job in jobs {
                if matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Running | JobStatus::Unknown
                ) {
                    report_job(engine, &job, JobStatus::Succeeded).await;
                }
            }
        }
    }

    #[tokio::test]
    async fn start_build_creates_queued_build_with_snapshot_and_event() {
        let (_dir, _pool, engine) = fixture().await;
        let mut rx = engine.bus().subscribe();
        let pipeline = pipeline();
        let row = save_and_start(&engine, pipeline.clone(), Some("main")).await;

        assert_eq!(row.status, BuildStatus::Queued);
        assert_eq!(row.number, 1);
        assert_eq!(row.trigger, TriggerSource::Manual);
        assert_eq!(row.attempt, 1);
        // 快照：整份定义 + 所用 revision，读回与保存时等价。
        let snapshot: BuildSnapshot = serde_json::from_str(&row.snapshot).expect("快照应可解析");
        assert_eq!(snapshot.pipeline, pipeline);
        assert_eq!(snapshot.revision.number, 1);

        // BuildCreated 事件广播。
        assert!(matches!(
            rx.try_recv().expect("事件"),
            Event::BuildCreated { build_id, number, trigger: TriggerSource::Manual, .. }
                if build_id == row.id && number == 1
        ));

        // 定义保存不影响已入队/运行中构建：改定义再触发，新构建拿新 revision。
        let mut changed = pipeline.clone();
        changed.stages[0].jobs[0].name = "compile-v2".into();
        engine
            .pipelines
            .save("demo", "release", &changed, "tester")
            .await
            .expect("改定义");
        let second = engine
            .start_build(StartBuildInput {
                project_name: "demo".into(),
                pipeline_name: "release".into(),
                trigger: TriggerSource::Cron,
                detail: trigger(None),
            })
            .await
            .expect("二触");
        assert_eq!(second.number, 2);
        assert_eq!(second.trigger, TriggerSource::Cron);
        let second_snapshot: BuildSnapshot = serde_json::from_str(&second.snapshot).expect("快照");
        assert_eq!(second_snapshot.revision.number, 2, "新构建拿新 revision");
        // 原构建快照不变（快照语义：入队即定）。
        let stored: BuildSnapshot = serde_json::from_str(&row.snapshot).expect("快照");
        assert_eq!(stored.pipeline.stages[0].jobs[0].name, "compile");
    }

    /// AC：阶段按序、阶段内并行；阶段级 when 不满足整阶段跳过（其内任务
    /// 全不发）；任务级 when 求值正确。
    #[tokio::test]
    async fn drive_advances_stages_spawning_parallel_jobs_and_skips_when_false() {
        let (_dir, _pool, engine) = fixture().await;
        let row = save_and_start(&engine, pipeline(), Some("main")).await;

        // 排队 → 运行（FIFO 放行 + 阶段 0 下发两个任务）。
        engine.drive(row.id).await.expect("推进");
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Running);
        assert!(build.started_at.is_some());
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(jobs.len(), 2, "阶段 0 两个任务并行下发");
        assert_eq!(job_by_name(&jobs, "compile").status, JobStatus::Queued);
        assert_eq!(job_by_name(&jobs, "lint").status, JobStatus::Queued);

        // 任务级 when：lint 配 `${SISY_BRANCH} == "main"`，main 分支 → 跑。
        engine.drive(row.id).await.expect("任务级 when 求值");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(job_by_name(&jobs, "lint").status, JobStatus::Queued);

        // 阶段 0 全部成功 → 阶段 1 下发（阶段串行、阶段内并行）。
        let compile = job_by_name(&jobs, "compile").clone();
        let lint = job_by_name(&jobs, "lint").clone();
        report_job(&engine, &compile, JobStatus::Succeeded).await;
        report_job(&engine, &lint, JobStatus::Succeeded).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(jobs.len(), 3, "阶段 1 unit 已下发");
        assert_eq!(job_by_name(&jobs, "unit").status, JobStatus::Queued);
        assert_eq!(job_by_name(&jobs, "unit").attempt, 1);

        // 阶段 1 成功 → 阶段 2 when `${SISY_BRANCH} == "prod"` 不满足 →
        // 整阶段跳过（其内任务全不发）。
        let unit = job_by_name(&jobs, "unit").clone();
        report_job(&engine, &unit, JobStatus::Succeeded).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(jobs.len(), 4, "deploy 阶段任务以 Skipped 落行");
        let publish = job_by_name(&jobs, "publish");
        assert_eq!(
            publish.status,
            JobStatus::Skipped,
            "阶段级 when 不满足整阶段跳过"
        );
        assert!(publish.spec_json.is_none(), "跳过的任务全不发（无 spec）");

        // 全部终态 → 构建 succeeded。
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Succeeded);
        assert!(build.finished_at.is_some());
    }

    /// AC：任务级 when 不满足 → 该任务 skipped（同阶段其它任务照跑）。
    #[tokio::test]
    async fn job_level_when_false_skips_only_that_job() {
        let (_dir, _pool, engine) = fixture().await;
        // lint 的 when 是 main 分支；dev 分支触发 → lint skipped。
        let row = save_and_start(&engine, pipeline(), Some("dev")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(job_by_name(&jobs, "compile").status, JobStatus::Queued);
        assert_eq!(job_by_name(&jobs, "lint").status, JobStatus::Skipped);

        // compile 成功 → 阶段推进不受 skipped 影响。
        let compile = job_by_name(&jobs, "compile").clone();
        report_job(&engine, &compile, JobStatus::Succeeded).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(job_by_name(&jobs, "unit").status, JobStatus::Queued);
    }

    /// AC：fail-fast——任务失败且非 allow_failure → 同阶段未完成任务
    /// cancelled、后续阶段 skipped、构建 failed。
    #[tokio::test]
    async fn fail_fast_cascades_on_non_allow_failure_failure() {
        let (_dir, _pool, engine) = fixture().await;
        let row = save_and_start(&engine, pipeline(), Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile").clone();

        // 阶段 0 在跑时某任务失败（非豁免）→ 级联：构建 failed、阶段 1/2
        // 不再下发。
        report_job(&engine, &compile, JobStatus::Failed).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed);
        assert!(build.finished_at.is_some());

        // 后续 drive：构建已 failed，不再下发阶段 1。
        engine.drive(row.id).await.expect("幂等推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(jobs.len(), 2, "阶段 1/2 不再下发");
        // 同阶段未完成任务（lint）被级联取消。
        assert_eq!(job_by_name(&jobs, "lint").status, JobStatus::Cancelled);
    }

    /// AC：allow_failure 豁免成立——失败任务不触发级联，构建照常推进。
    #[tokio::test]
    async fn allow_failure_exempts_from_fail_fast() {
        let (_dir, _pool, engine) = fixture().await;
        let mut pipeline = pipeline();
        pipeline.stages[0].jobs[1].allow_failure = true;
        let row = save_and_start(&engine, pipeline, Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile").clone();
        let lint = job_by_name(&jobs, "lint").clone();

        report_job(&engine, &compile, JobStatus::Succeeded).await;
        report_job(&engine, &lint, JobStatus::Failed).await;

        // 豁免：构建不 failed，阶段 1 继续下发。
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(
            build.status,
            BuildStatus::Running,
            "allow_failure 不触发级联"
        );
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(job_by_name(&jobs, "unit").status, JobStatus::Queued);

        // 后续全部成功 → 构建 succeeded（豁免任务的失败保留）。
        let unit = job_by_name(&jobs, "unit").clone();
        report_job(&engine, &unit, JobStatus::Succeeded).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Succeeded);
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(job_by_name(&jobs, "lint").status, JobStatus::Failed);
    }

    /// AC：自动重试——任务失败且 retry_count 未耗尽 → 同 job 新 attempt
    /// 重新入池；耗尽才 failed。
    #[tokio::test]
    async fn auto_retry_reenqueues_same_job_until_exhausted() {
        let (_dir, _pool, engine) = fixture().await;
        // 阶段 1 unit 配 retry_count=1：首败重试一次，再败才级联。
        let row = save_and_start(&engine, pipeline(), Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        for name in ["compile", "lint"] {
            let job = job_by_name(&jobs, name).clone();
            report_job(&engine, &job, JobStatus::Succeeded).await;
        }
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let unit = job_by_name(&jobs, "unit").clone();
        assert_eq!(unit.retry_count, 1);

        // 首次失败 → 重试：同 job 新 attempt=2 重新入池，构建不 failed。
        report_job(&engine, &unit, JobStatus::Failed).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let retry = latest_job_by_name(&jobs, "unit");
        assert_eq!(retry.attempt, 2, "同 job 新 attempt");
        assert_eq!(retry.status, JobStatus::Queued, "重新入池");
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Running, "重试不级联");

        // 重试又败 → 耗尽：级联，构建 failed。
        report_job(&engine, retry, JobStatus::Failed).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed, "耗尽才 failed");
        // 历史保留：attempt=1 与 attempt=2 两行都在。
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let attempts: Vec<i32> = jobs
            .iter()
            .filter(|j| j.name == "unit")
            .map(|j| j.attempt)
            .collect();
        assert_eq!(attempts, vec![1, 2], "重试历史保留");
    }

    /// AC：ResolvedJobSpec 组装——变量替换（含 SISY_WORKSPACE 占位）、env
    /// 覆盖合并、隐式容器标签、只含待跑节点，产物落 spec 快照。
    #[tokio::test]
    async fn resolved_spec_is_assembled_and_snapshotted() {
        let (_dir, _pool, engine) = fixture().await;
        let row = save_and_start(&engine, pipeline(), Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile");
        let spec_json = compile.spec_json.as_ref().expect("spec 快照已落库");
        let spec: ResolvedJobSpec = serde_json::from_str(spec_json).expect("spec 应可解析");

        assert_eq!(spec.pipeline_name, "release");
        assert_eq!(spec.job_name, "compile");
        assert_eq!(spec.build_number, row.number);
        assert_eq!(spec.attempt, 1);
        assert_eq!(spec.timeout_minutes, 30);

        // 变量替换：内置 + 参数；SISY_WORKSPACE 占位保留。
        let env: HashMap<&str, &str> = spec
            .env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(env["CARGO_HOME"], "${SISY_WORKSPACE}/.cargo");
        assert_eq!(
            env["MODE"], "x86_64",
            "任务级 env 引用参数 target 替换为默认值"
        );
        let spec::ResolvedStep::Shell { command, .. } = &spec.steps[0] else {
            panic!("compile 第一步应为 shell");
        };
        assert_eq!(command, "echo 1 x86_64");
        assert_eq!(spec.steps.len(), 1, "只含待跑节点");

        // 任务级 when 求值：lint 用 main 分支 → 下发并含 spec。
        let lint = job_by_name(&jobs, "lint");
        assert!(lint.spec_json.is_some());

        // 阶段 1 unit 是容器任务 → 隐式容器标签。
        let compile_job = job_by_name(&jobs, "compile").clone();
        report_job(&engine, &compile_job, JobStatus::Succeeded).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let lint_job = job_by_name(&jobs, "lint").clone();
        report_job(&engine, &lint_job, JobStatus::Succeeded).await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let unit = job_by_name(&jobs, "unit");
        let unit_spec: ResolvedJobSpec =
            serde_json::from_str(unit.spec_json.as_ref().expect("spec")).expect("解析");
        assert!(
            unit_spec
                .labels
                .contains(&"sisyphus/container=docker".to_string()),
            "容器任务隐式追加容器标签"
        );
    }

    /// AC：任务引用不存在的机密名 → 该任务立即 failed（detail 记名）且
    /// 级联 fail-fast。
    #[tokio::test]
    async fn missing_secret_fails_job_with_name_and_cascades() {
        let (_dir, _pool, engine) = fixture().await;
        let mut pipeline = pipeline();
        pipeline.stages[0].jobs[0].secrets = vec!["DEPLOY_KEY".into()];
        let row = save_and_start(&engine, pipeline, Some("main")).await;
        engine.drive(row.id).await.expect("推进");

        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile");
        assert_eq!(compile.status, JobStatus::Failed, "缺失机密的任务立即失败");
        let detail = compile.detail.as_deref().expect("detail 记名");
        assert!(
            detail.contains("DEPLOY_KEY"),
            "detail 应含缺失机密名：{detail}"
        );
        assert!(compile.spec_json.is_none(), "失败任务不落 spec");

        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed, "级联 fail-fast");
    }

    /// AC：机密按名解密注入 env（存在即注入；值与密文同库 round-trip）。
    #[tokio::test]
    async fn existing_secret_is_decrypted_into_env() {
        let (_dir, pool, _engine) = fixture().await;
        let key_path = _dir.path().join("master.key");
        let key = crate::secrets::ensure_master_key(&key_path).expect("密钥");
        let engine_with_key = Engine::new(pool.clone(), key, EventBus::new());
        let blob = crate::secrets::encrypt(&key, b"deploy-value").expect("加密");
        engine_with_key
            .secrets
            .upsert(1, "DEPLOY_KEY", &blob, "alice", 0)
            .await
            .expect("建机密");

        let mut pipeline = pipeline();
        pipeline.stages[0].jobs[0].secrets = vec!["DEPLOY_KEY".into()];
        let row = save_and_start(&engine_with_key, pipeline, Some("main")).await;
        engine_with_key.drive(row.id).await.expect("推进");

        let jobs = engine_with_key
            .jobs
            .list_by_build(row.id)
            .await
            .expect("任务清单");
        let compile = job_by_name(&jobs, "compile");
        assert_eq!(compile.status, JobStatus::Queued);
        let spec: ResolvedJobSpec =
            serde_json::from_str(compile.spec_json.as_ref().expect("spec")).expect("解析");
        assert!(spec.secrets.contains(&"DEPLOY_KEY".to_string()));
        let env: HashMap<&str, &str> = spec
            .env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(env["DEPLOY_KEY"], "deploy-value", "机密解密注入 env");
    }

    /// AC：FIFO——同 pipeline 同时只跑一条，后来者排队；驱动只放行队头。
    #[tokio::test]
    async fn fifo_serializes_builds_and_drive_promotes_oldest() {
        let (_dir, _pool, engine) = fixture().await;
        let first = save_and_start(&engine, pipeline(), Some("main")).await;
        let second = save_and_start(&engine, pipeline(), Some("main")).await;

        // 驱动第一条：放行（无运行中）+ 阶段下发；第二条保持 queued。
        engine.drive(first.id).await.expect("推进第一条");
        let a = engine
            .builds
            .get(first.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(a.status, BuildStatus::Running);
        let b = engine
            .builds
            .get(second.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(b.status, BuildStatus::Queued, "后来者排队");

        // 驱动第二条：已有运行中构建 → 不放行（仍 queued）。
        engine.drive(second.id).await.expect("驱动第二条");
        let b = engine
            .builds
            .get(second.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(b.status, BuildStatus::Queued);

        // 第一条跑完 → 第二条接力（FIFO 串行队列）。
        complete_build_ok(&engine, first.id).await;
        engine.drive(second.id).await.expect("接力");
        let b = engine
            .builds
            .get(second.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(b.status, BuildStatus::Running, "第一条终态后接力");
    }

    /// AC：从头重跑（新号 attempt=1）与从失败任务重跑（同号 attempt+1、
    /// 成功任务保留）两种语义可用。
    #[tokio::test]
    async fn rerun_from_failed_resumes_with_preserved_successes() {
        let (_dir, _pool, engine) = fixture().await;
        let row = save_and_start(&engine, pipeline(), Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile").clone();
        let lint = job_by_name(&jobs, "lint").clone();

        // compile 成功、lint 失败 → 构建 failed。
        report_job(&engine, &compile, JobStatus::Succeeded).await;
        report_job(&engine, &lint, JobStatus::Failed).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed);

        // 从失败任务重跑：同号 attempt+1、回 queued。
        let rerun = engine
            .builds
            .rerun_from_failed(row.id)
            .await
            .expect("重跑")
            .expect("应可重跑");
        assert_eq!(rerun.number, row.number, "同号延续");
        assert_eq!(rerun.attempt, 2, "attempt+1");
        assert_eq!(rerun.status, BuildStatus::Queued);

        // 驱动重跑构建：成功任务保留、失败任务重开 attempt=2（失败任务起继续）。
        engine.drive(rerun.id).await.expect("推进重跑");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile_now = job_by_name(&jobs, "compile");
        let lint_now = latest_job_by_name(&jobs, "lint");
        assert_eq!(compile_now.attempt, 1, "成功任务保留（attempt=1 原行）");
        assert_eq!(compile_now.status, JobStatus::Succeeded);
        assert_eq!(lint_now.attempt, 2, "失败任务按新 attempt 重开");
        assert_eq!(lint_now.status, JobStatus::Queued, "重跑从失败任务起继续");

        // 重跑构建最终可成功（补充路径完整性）。
        complete_build_ok(&engine, rerun.id).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Succeeded, "重跑续跑至成功");
    }

    /// code-review 回归：从失败任务重跑时，阶段级 when 重新求值——之前被
    /// 整阶段跳过的阶段（when 不满足）其任务依然不发，不因重跑重开。
    #[tokio::test]
    async fn rerun_keeps_when_false_stage_skipped() {
        let (_dir, _pool, engine) = fixture().await;
        let row = save_and_start(&engine, pipeline(), Some("main")).await;
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile").clone();
        let lint = job_by_name(&jobs, "lint").clone();

        // compile 成功、lint 失败 → 构建 failed（阶段 2 deploy 从未下发）。
        report_job(&engine, &compile, JobStatus::Succeeded).await;
        report_job(&engine, &lint, JobStatus::Failed).await;
        let build = engine
            .builds
            .get(row.id)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(build.status, BuildStatus::Failed);

        // 从失败任务重跑：同号 attempt+1。
        let rerun = engine
            .builds
            .rerun_from_failed(row.id)
            .await
            .expect("重跑")
            .expect("应可重跑");

        // 驱动重跑：阶段 0 lint 重开 attempt=2（compile 成功保留）；阶段 1
        // 待 lint 完成后才下发（阶段串行）。
        engine.drive(rerun.id).await.expect("推进重跑");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(latest_job_by_name(&jobs, "lint").attempt, 2);
        assert!(
            !jobs.iter().any(|j| j.name == "unit"),
            "阶段 1 待 lint 完成后才下发"
        );

        // lint 重跑成功 → 阶段 1 unit 下发。
        report_job(
            &engine,
            latest_job_by_name(&jobs, "lint"),
            JobStatus::Succeeded,
        )
        .await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        assert_eq!(latest_job_by_name(&jobs, "unit").status, JobStatus::Queued);

        // 重跑跑通阶段 0/1 后：阶段 2 deploy 的 when 仍不满足（main 分支）
        // → 整阶段保持跳过，其任务全不发（无 queued 行）。
        report_job(
            &engine,
            latest_job_by_name(&jobs, "unit"),
            JobStatus::Succeeded,
        )
        .await;
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let publish = latest_job_by_name(&jobs, "publish");
        assert_eq!(
            publish.status,
            JobStatus::Skipped,
            "重跑后 when 不满足的阶段仍跳过"
        );
        assert!(publish.spec_json.is_none(), "跳过的任务全不发");
        assert_eq!(
            engine
                .builds
                .get(row.id)
                .await
                .expect("查")
                .expect("应存在")
                .status,
            BuildStatus::Succeeded,
            "重跑续跑至成功"
        );
    }

    /// AC：poll 触发源与手动/cron 一样统一走 start_build（快照含轮询到的
    /// 提交、构建号按序、触发源落库）。
    #[tokio::test]
    async fn poll_trigger_source_runs_through_start_build() {
        let (_dir, _pool, engine) = fixture().await;
        engine
            .pipelines
            .save("demo", "release", &pipeline(), "tester")
            .await
            .expect("保存定义");
        let row = engine
            .start_build(StartBuildInput {
                project_name: "demo".into(),
                pipeline_name: "release".into(),
                trigger: TriggerSource::Poll,
                detail: TriggerDetail {
                    by: "poll".into(),
                    branch: Some("main".into()),
                    commit: Some("deadbeef".into()),
                    revision: None,
                    params: vec![],
                },
            })
            .await
            .expect("poll 触发");

        assert_eq!(row.trigger, TriggerSource::Poll);
        assert_eq!(row.number, 1);
        let detail: TriggerDetail =
            serde_json::from_str(&row.trigger_detail).expect("触发上下文可解析");
        assert_eq!(
            detail.commit.as_deref(),
            Some("deadbeef"),
            "poll 上下文含轮询提交"
        );

        // 驱动后组装出的 spec 钉到轮询提交（SCM 上下文）。
        engine.drive(row.id).await.expect("推进");
        let jobs = engine.jobs.list_by_build(row.id).await.expect("任务清单");
        let compile = job_by_name(&jobs, "compile");
        let spec: ResolvedJobSpec =
            serde_json::from_str(compile.spec_json.as_ref().expect("spec")).expect("解析");
        assert_eq!(spec.scm.commit, "deadbeef");
    }

    #[tokio::test]
    async fn start_build_missing_project_or_pipeline_is_not_found() {
        let (_dir, _pool, engine) = fixture().await;
        let err = engine
            .start_build(StartBuildInput {
                project_name: "nope".into(),
                pipeline_name: "release".into(),
                trigger: TriggerSource::Manual,
                detail: trigger(None),
            })
            .await
            .expect_err("项目不存在应报错");
        assert!(matches!(err, StoreError::NotFound(_)));

        engine
            .pipelines
            .save("demo", "release", &pipeline(), "tester")
            .await
            .expect("存定义");
        let err = engine
            .start_build(StartBuildInput {
                project_name: "demo".into(),
                pipeline_name: "nope".into(),
                trigger: TriggerSource::Manual,
                detail: trigger(None),
            })
            .await
            .expect_err("pipeline 不存在应报错");
        assert!(matches!(err, StoreError::NotFound(_)));
    }
}

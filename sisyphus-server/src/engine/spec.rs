//! ResolvedJobSpec 组装（票 #46，ADR-0009：proto JobSpec 的源，组装产物存
//! jobs.spec_json 快照、审计「当时下发什么」）。
//!
//! 组装语义（ADR-0006/0008/0011/0015/0016）：
//! - 变量替换完毕：7 个 Server 解析内置变量 + 参数；`SISY_WORKSPACE` 以
//!   占位符原样保留（Agent 执行前替换，ADR-0011），when 禁用它由保存校验
//!   保证（B1）。
//! - env 合并完毕：pipeline 级 → 任务级覆盖同名 → 机密按名解密注入（键
//!   冲突被保存校验拒绝，ADR-0015）。
//! - 三级 when 求值完毕：阶段级（engine 裁决跳过整级）+ 任务级 + 步骤级
//!   在此过滤，规格只含待跑节点。
//! - 隐式容器标签：容器执行环境追加 `sisyphus/container=docker`（ADR-0008）。
//! - SCM 上下文：git ref + commit / svn revision，随 checkout 步骤下发。
//!
//! 本模块只做纯组装（可单测）；机密密文的获取在 engine 层（store 缝），
//! 这里拿到密文后解密注入。解密失败/缺失记入 [`AssembleError`]。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sisyphus_model::pipeline::{
    ArtifactDownload, ArtifactUpload, CacheSpec, EnvVar, ExecutionEnv, Job, Stage,
};
use sisyphus_model::validate::BuildSnapshot;
use sisyphus_model::variables::{Resolver, UndefinedPolicy};
use sisyphus_model::when;

use crate::secrets::MasterKey;
use crate::store::builds::BuildRow;
use crate::store::projects::{Project, ScmType};

use super::TriggerDetail;

/// SCM 版本控制类型（spec 快照的序列化形态；store 的 ScmType 无 serde）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    /// git。
    Git,
    /// svn。
    Svn,
}

/// SCM 上下文（ADR-0016：每次构建都有明确的 SCM 上下文；随规格下发供
/// checkout 步骤钉到确切提交）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmContext {
    /// 版本控制类型。
    pub vcs: Vcs,
    /// 项目绑定仓库 URL。
    pub repo_url: String,
    /// git 分支（手动可选、缺省项目默认分支；svn 无分支概念为空）。
    pub branch: String,
    /// git commit sha（手动未钉提交为空——Agent 检分支头；poll 为轮询提交）。
    pub commit: String,
    /// svn revision（手动可选；git 为空）。
    pub revision: String,
}

/// 已求值的步骤（只含待跑节点；`seq` 为步骤序号，shell 命令已变量替换）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "config", rename_all = "snake_case")]
pub enum ResolvedStep {
    /// shell 步骤。
    Shell {
        /// 步骤序号（任务内单调）。
        seq: i32,
        /// 已解析命令（`${}` 替换完毕，`SISY_WORKSPACE` 占位保留）。
        command: String,
    },
    /// checkout scm 步骤。
    Checkout {
        /// 步骤序号（任务内单调）。
        seq: i32,
        /// SCM 上下文（检出到工作区根并钉到确切提交）。
        scm: ScmContext,
        /// git 子模块开关（默认开，ADR-0016）。
        submodules: bool,
    },
}

/// 组装好的任务规格（jobs.spec_json 的模型形态；映射 proto JobSpec 下发，
/// job_id/log_limit_bytes/scm_credential 等下发期字段不入快照）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedJobSpec {
    /// pipeline 名（仅标识/工作区路径）。
    pub pipeline_name: String,
    /// 任务名（仅标识/工作区路径）。
    pub job_name: String,
    /// 阶段名。
    pub stage_name: String,
    /// 阶段序号（快照内阶段数组下标）。
    pub stage_index: i32,
    /// per-pipeline 自增构建号。
    pub build_number: i64,
    /// 重跑 attempt。
    pub attempt: i32,
    /// 已合并 env（pipeline → 任务覆盖 → 机密注入；注入进程环境）。
    pub env: Vec<EnvVar>,
    /// Agent 标签要求（含隐式容器标签；AND 全集匹配，调度后回显）。
    pub labels: Vec<String>,
    /// 注入的机密名清单（审计「哪些凭据随任务」，值只随 env）。
    pub secrets: Vec<String>,
    /// 执行环境（host / container）。
    pub exec_env: ExecutionEnv,
    /// 任务超时（分钟，0 = 无限）。
    pub timeout_minutes: i64,
    /// 自动重试次数（耗尽仍失败才算失败）。
    pub retry_count: i64,
    /// allow_failure 豁免 fail-fast。
    pub allow_failure: bool,
    /// 待执行步骤（三级 when 求值后只含待跑节点）。
    pub steps: Vec<ResolvedStep>,
    /// 产物上传声明（完成后上传）。
    pub artifact_uploads: Vec<ArtifactUpload>,
    /// 产物下载依赖（开始前拉取，仅限本次构建内其它任务）。
    pub artifact_downloads: Vec<ArtifactDownload>,
    /// 缓存声明（key 模板已变量替换）。
    pub caches: Vec<CacheSpec>,
    /// SCM 上下文。
    pub scm: ScmContext,
}

/// 组装错误（机密缺失/解密失败——任务下发前立即失败，走 fail-fast）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssembleError {
    /// 任务引用的机密名在项目中不存在（detail 记名）。
    MissingSecret(Vec<String>),
    /// 机密密文解密失败（密文损坏或主密钥不匹配）。
    Decrypt(String),
}

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssembleError::MissingSecret(names) => {
                write!(f, "缺少机密：{}", names.join(", "))
            }
            AssembleError::Decrypt(name) => write!(f, "机密解密失败：{name}"),
        }
    }
}

/// 组装输入（engine 层组装调用侧提供；密文已按项目批量取回）。
pub struct AssembleInput<'a> {
    /// 属主构建行。
    pub build: &'a BuildRow,
    /// 构建快照（整份定义，env/参数等组装来源）。
    pub snapshot: &'a BuildSnapshot,
    /// 当前阶段序号。
    pub stage_index: usize,
    /// 当前阶段定义。
    pub stage: &'a Stage,
    /// 任务定义。
    pub job: &'a Job,
    /// 属主项目（SCM 上下文来源）。
    pub project: &'a Project,
    /// 触发上下文（分支/commit/revision）。
    pub trigger: &'a TriggerDetail,
    /// 参数值（默认值 + 手动覆盖合并）。
    pub params: &'a HashMap<String, String>,
    /// 任务声明机密名的密文（缺名不在其中 → 缺失）。
    pub secret_ciphertexts: &'a HashMap<String, Vec<u8>>,
    /// 机密解密主密钥。
    pub master_key: &'a MasterKey,
}

/// 组装一份任务规格。机密缺失/解密失败返回 [`AssembleError]`（调用侧
/// 立即失败任务并级联）；其余组装步骤不失败（变量替换是 Keep 策略）。
pub fn assemble(input: &AssembleInput<'_>) -> Result<ResolvedJobSpec, AssembleError> {
    let scm = scm_context(input.project, input.trigger);
    let var_env = var_env(
        input.build,
        input.project,
        input.stage,
        Some(input.job),
        &scm,
        input.params,
    );
    let lookup = |name: &str| var_env.get(name).cloned();
    let mut env = Vec::new();
    let mut missing = Vec::new();
    let mut secrets = Vec::new();
    for name in &input.job.secrets {
        let Some(ciphertext) = input.secret_ciphertexts.get(name) else {
            missing.push(name.clone());
            continue;
        };
        let plaintext = crate::secrets::decrypt(input.master_key, ciphertext)
            .map_err(|_| AssembleError::Decrypt(name.clone()))?;
        let value = String::from_utf8_lossy(&plaintext).into_owned();
        env.push(EnvVar {
            name: name.clone(),
            value,
        });
        secrets.push(name.clone());
    }
    if !missing.is_empty() {
        return Err(AssembleError::MissingSecret(missing));
    }

    // env 合并：pipeline 级（解析）→ 任务级同名覆盖（解析）→ 机密追加
    // （机密名与 env 键冲突已被保存校验拒绝，ADR-0015）。
    let pipeline_env = input
        .snapshot
        .pipeline
        .env
        .iter()
        .map(|e| resolve_env(e, &lookup))
        .collect::<Vec<_>>();
    let job_env = input
        .job
        .env
        .iter()
        .map(|e| resolve_env(e, &lookup))
        .collect::<Vec<_>>();
    let mut merged = merge_env(pipeline_env, job_env);
    merged.extend(env);

    // 标签：任务声明 + 隐式容器标签（ADR-0008：容器任务调度器隐式追加）。
    let mut labels = input.job.labels.clone();
    if matches!(input.job.exec_env, Some(ExecutionEnv::Container { .. }))
        && !labels.iter().any(|l| l == "sisyphus/container=docker")
    {
        labels.push("sisyphus/container=docker".into());
    }

    // 步骤：步骤级 when 求值（快照定义必已过保存校验），只留待跑节点。
    let mut steps = Vec::new();
    for (seq, step) in input.job.steps.iter().enumerate() {
        if let Some(source) = step.when()
            && !eval_when(source, &var_env, &input.job.name, &input.stage.name)
        {
            continue;
        }
        let resolved = match step {
            sisyphus_model::pipeline::Step::Shell { command, .. } => {
                let (command, _) = Resolver::new(lookup, UndefinedPolicy::Keep).resolve(command);
                ResolvedStep::Shell {
                    seq: seq as i32,
                    command,
                }
            }
            sisyphus_model::pipeline::Step::Checkout { submodules, .. } => {
                ResolvedStep::Checkout {
                    seq: seq as i32,
                    scm: scm.clone(),
                    submodules: *submodules,
                }
            }
        };
        steps.push(resolved);
    }

    // 产物/缓存字符串字段变量替换（Keep：SISY_WORKSPACE 保留占位）。
    let resolve = |s: &str| {
        let (out, _) = Resolver::new(lookup, UndefinedPolicy::Keep).resolve(s);
        out
    };
    let artifact_uploads = input
        .job
        .artifact_uploads
        .iter()
        .map(|u| ArtifactUpload {
            name: resolve(&u.name),
            path: resolve(&u.path),
        })
        .collect();
    let artifact_downloads = input
        .job
        .artifact_downloads
        .iter()
        .map(|d| ArtifactDownload {
            job: resolve(&d.job),
            name: resolve(&d.name),
            path: resolve(&d.path),
        })
        .collect();
    let caches = input
        .job
        .caches
        .iter()
        .map(|c| CacheSpec {
            key: resolve(&c.key),
            paths: c.paths.clone(),
            files: c.files.clone(),
        })
        .collect();

    Ok(ResolvedJobSpec {
        pipeline_name: input.build.pipeline_name.clone(),
        job_name: input.job.name.clone(),
        stage_name: input.stage.name.clone(),
        stage_index: input.stage_index as i32,
        build_number: input.build.number,
        attempt: input.build.attempt,
        env: merged,
        labels,
        secrets,
        exec_env: input.job.exec_env.clone().unwrap_or_default(),
        timeout_minutes: input.job.timeout_minutes as i64,
        retry_count: input.job.retry_count as i64,
        allow_failure: input.job.allow_failure,
        steps,
        artifact_uploads,
        artifact_downloads,
        caches,
        scm,
    })
}

/// SCM 上下文：git 取 分支（缺省项目默认分支）+ commit；svn 取 revision。
pub(crate) fn scm_context(project: &Project, trigger: &TriggerDetail) -> ScmContext {
    match project.scm_type {
        ScmType::Git => ScmContext {
            vcs: Vcs::Git,
            repo_url: project.scm_url.clone(),
            branch: trigger
                .branch
                .clone()
                .or_else(|| project.default_branch.clone())
                .unwrap_or_default(),
            commit: trigger.commit.clone().unwrap_or_default(),
            revision: String::new(),
        },
        ScmType::Svn => ScmContext {
            vcs: Vcs::Svn,
            repo_url: project.scm_url.clone(),
            branch: String::new(),
            commit: String::new(),
            revision: trigger.revision.clone().unwrap_or_default(),
        },
    }
}

/// 求值/替换环境：参数（默认 + 覆盖）与 7 个 Server 解析内置变量的合并
/// 视图。`SISY_WORKSPACE` 不注入（占位符保留语义）；`SISY_COMMIT_ID` 只在
/// 钉了提交/revision 时定义（`exists SISY_COMMIT_ID` 语义）。`job` 只在
/// 任务级求值/组装时给（阶段级 when 求值传 `None`，此时 `SISY_JOB_NAME`
/// 未定义——`exists SISY_JOB_NAME` 的语义天然为假，不误伤阶段级）。
pub(crate) fn var_env(
    build: &BuildRow,
    project: &Project,
    stage: &Stage,
    job: Option<&Job>,
    scm: &ScmContext,
    params: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("SISY_BUILD_NUMBER".into(), build.number.to_string());
    env.insert("SISY_PIPELINE_NAME".into(), build.pipeline_name.clone());
    env.insert("SISY_PROJECT_NAME".into(), project.name.clone());
    env.insert("SISY_STAGE_NAME".into(), stage.name.clone());
    env.insert("SISY_BRANCH".into(), scm.branch.clone());
    if let Some(job) = job {
        env.insert("SISY_JOB_NAME".into(), job.name.clone());
    }
    if !scm.commit.is_empty() {
        env.insert("SISY_COMMIT_ID".into(), scm.commit.clone());
    } else if !scm.revision.is_empty() {
        env.insert("SISY_COMMIT_ID".into(), scm.revision.clone());
    }
    for (name, value) in params {
        // 内置变量优先：系统变量不被用户参数遮蔽（保存校验未禁同名参数）。
        env.entry(name.clone()).or_insert_with(|| value.clone());
    }
    env
}

/// 求值一段 when 源码（快照内定义必已过保存校验：语法合法、禁用
/// `SISY_WORKSPACE`）。求值环境含全部 8 个内置变量中的 7 个（`SISY_WORKSPACE`
/// 不在——when 禁用）、参数与覆盖。求值失败（未定义变量等）按「条件不
/// 满足」处理：保存校验保证的定义在完整环境里可求值，失败只可能来自
/// 快照损坏，跳过比让整条构建挂死更稳（事件留 trace）。
pub(crate) fn eval_when(
    source: &str,
    env: &HashMap<String, String>,
    job_name: &str,
    stage_name: &str,
) -> bool {
    let expr = match when::parse(source) {
        Ok(expr) => expr,
        Err(_) => {
            tracing::warn!(job_name, stage_name, "快照内 when 语法非法，按不满足跳过");
            return false;
        }
    };
    match when::eval(&expr, &when::MapEnv(env)) {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(job_name, stage_name, "快照内 when 求值失败，按不满足跳过");
            false
        }
    }
}

/// 单条 env 值变量替换（Keep 策略：SISY_WORKSPACE 占位保留）。
fn resolve_env(e: &EnvVar, lookup: &impl Fn(&str) -> Option<String>) -> EnvVar {
    let (value, _) = Resolver::new(lookup, UndefinedPolicy::Keep).resolve(&e.value);
    EnvVar {
        name: e.name.clone(),
        value,
    }
}

/// env 合并：任务级覆盖 pipeline 级同名项（ADR-0006）。
fn merge_env(pipeline: Vec<EnvVar>, job: Vec<EnvVar>) -> Vec<EnvVar> {
    let mut merged = pipeline;
    for e in job {
        if let Some(existing) = merged.iter_mut().find(|m| m.name == e.name) {
            existing.value = e.value;
        } else {
            merged.push(e);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::ScmType;
    use sisyphus_model::pipeline::{Parameter, ParameterType, ParameterValue, Revision, Shell, Step};

    fn project() -> Project {
        Project {
            id: 1,
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn build() -> BuildRow {
        BuildRow {
            id: 1,
            project_id: 1,
            pipeline_name: "release".into(),
            number: 7,
            status: crate::store::builds::BuildStatus::Running,
            trigger: crate::store::builds::TriggerSource::Manual,
            trigger_detail: "{}".into(),
            attempt: 1,
            snapshot: String::new(),
            started_at: None,
            finished_at: None,
            cancelled_at: None,
            updated_at: 0,
        }
    }

    fn pipeline() -> sisyphus_model::pipeline::Pipeline {
        sisyphus_model::pipeline::Pipeline {
            name: "release".into(),
            parameters: vec![Parameter {
                name: "target".into(),
                r#type: ParameterType::Enum,
                required: true,
                default: Some(ParameterValue::String("x86_64".into())),
                description: None,
                choices: vec!["x86_64".into(), "aarch64".into()],
            }],
            env: vec![EnvVar {
                name: "CARGO_HOME".into(),
                value: "${SISY_WORKSPACE}/.cargo".into(),
            }],
            notification: None,
            stages: vec![stage()],
            revision: None,
        }
    }

    fn snapshot() -> BuildSnapshot {
        BuildSnapshot::new(
            pipeline(),
            Revision {
                number: 1,
                operator: "tester".into(),
                at_ms: 0,
            },
        )
    }

    fn stage() -> Stage {
        Stage {
            name: "build".into(),
            when: None,
            jobs: vec![job()],
        }
    }

    fn job() -> Job {
        Job {
            name: "compile".into(),
            exec_env: Some(ExecutionEnv::Container {
                image: "rust:1.97".into(),
            }),
            labels: vec!["sisyphus/os=linux".into()],
            when: None,
            env: vec![EnvVar {
                name: "MODE".into(),
                value: "release".into(),
            }],
            allow_failure: false,
            retry_count: 2,
            timeout_minutes: 30,
            artifact_uploads: vec![],
            artifact_downloads: vec![],
            caches: vec![],
            secrets: vec!["DEPLOY_KEY".into()],
            steps: vec![
                Step::Checkout {
                    submodules: true,
                    when: None,
                },
                Step::Shell {
                    command: "echo ${SISY_BUILD_NUMBER} ${SISY_WORKSPACE}/x ${target}".into(),
                    shell: Some(Shell::Bash),
                    when: Some("${SISY_BRANCH} == \"main\"".into()),
                },
                Step::Shell {
                    command: "echo skip-me".into(),
                    shell: None,
                    when: Some("${SISY_BRANCH} == \"dev\"".into()),
                },
            ],
        }
    }

    fn input(
        trigger: TriggerDetail,
        params: HashMap<String, String>,
        secrets: HashMap<String, Vec<u8>>,
    ) -> TestContext {
        TestContext {
            trigger,
            params,
            secret_ciphertexts: secrets,
            ..TestContext::base()
        }
    }

    /// 测试上下文：持有全部组装输入（生命期由本结构收拢，免借用临时值）。
    struct TestContext {
        build: BuildRow,
        snapshot: BuildSnapshot,
        stage: Stage,
        job: Job,
        project: Project,
        trigger: TriggerDetail,
        params: HashMap<String, String>,
        secret_ciphertexts: HashMap<String, Vec<u8>>,
        master_key: MasterKey,
    }

    impl TestContext {
        fn base() -> Self {
            Self {
                build: build(),
                snapshot: snapshot(),
                stage: stage(),
                job: job(),
                project: project(),
                trigger: TriggerDetail {
                    by: "alice".into(),
                    branch: Some("main".into()),
                    commit: None,
                    revision: None,
                    params: vec![],
                },
                params: HashMap::new(),
                secret_ciphertexts: HashMap::new(),
                master_key: MasterKey::generate(),
            }
        }

        /// 用本上下文的密钥写入一份机密（解密密钥一致，避免「错钥 → Decrypt」）。
        fn set_secret(&mut self, name: &str, value: &[u8]) {
            self.secret_ciphertexts.insert(
                name.into(),
                crate::secrets::encrypt(&self.master_key, value).expect("加密"),
            );
        }

        fn assemble(&self) -> Result<ResolvedJobSpec, AssembleError> {
            super::assemble(&AssembleInput {
                build: &self.build,
                snapshot: &self.snapshot,
                stage_index: 0,
                stage: &self.stage,
                job: &self.job,
                project: &self.project,
                trigger: &self.trigger,
                params: &self.params,
                secret_ciphertexts: &self.secret_ciphertexts,
                master_key: &self.master_key,
            })
        }
    }

    #[test]
    fn assembles_full_spec_with_env_merge_secrets_and_container_label() {
        let mut ctx = input(
            TriggerDetail {
                by: "alice".into(),
                branch: Some("main".into()),
                commit: Some("abc123".into()),
                revision: None,
                params: vec![],
            },
            HashMap::from([("target".into(), "aarch64".into())]),
            HashMap::new(),
        );
        ctx.set_secret("DEPLOY_KEY", b"deploy-value");
        let spec = ctx.assemble().expect("组装应成功");

        assert_eq!(spec.pipeline_name, "release");
        assert_eq!(spec.job_name, "compile");
        assert_eq!(spec.stage_name, "build");
        assert_eq!(spec.build_number, 7);
        assert_eq!(spec.attempt, 1);
        assert_eq!(spec.timeout_minutes, 30);
        assert_eq!(spec.retry_count, 2);
        assert!(!spec.allow_failure);

        // env 合并：pipeline 级 CARGO_HOME 保留占位、任务级 MODE、机密注入。
        let env: HashMap<&str, &str> = spec
            .env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(env["CARGO_HOME"], "${SISY_WORKSPACE}/.cargo", "SISY_WORKSPACE 占位保留");
        assert_eq!(env["MODE"], "release");
        assert_eq!(env["DEPLOY_KEY"], "deploy-value", "机密解密注入 env");

        // 机密名清单（审计）+ 隐式容器标签。
        assert_eq!(spec.secrets, vec!["DEPLOY_KEY"]);
        assert!(
            spec.labels.contains(&"sisyphus/container=docker".to_string()),
            "容器任务隐式追加容器标签"
        );
        assert!(spec.labels.contains(&"sisyphus/os=linux".to_string()));

        // 步骤：checkout（带 SCM 上下文）+ 步骤级 when 过滤后只剩 main 分支步骤。
        assert_eq!(spec.steps.len(), 2, "dev 分支步骤不发");
        assert!(matches!(
            &spec.steps[0],
            ResolvedStep::Checkout {
                scm: ScmContext { commit, branch, .. },
                submodules: true,
                ..
            } if commit == "abc123" && branch == "main"
        ));
        match &spec.steps[1] {
            ResolvedStep::Shell { command, .. } => {
                assert_eq!(command, "echo 7 ${SISY_WORKSPACE}/x aarch64");
            }
            other => panic!("第二步应为 shell：{other:?}"),
        }

        // SCM 上下文。
        assert_eq!(spec.scm.vcs, Vcs::Git);
        assert_eq!(spec.scm.repo_url, "https://example.com/repo");
        assert_eq!(spec.scm.branch, "main");
        assert_eq!(spec.scm.commit, "abc123");
    }

    #[test]
    fn missing_secret_is_reported_by_name() {
        let ctx = input(
            TriggerDetail {
                by: "alice".into(),
                branch: None,
                commit: None,
                revision: None,
                params: vec![],
            },
            HashMap::new(),
            HashMap::new(),
        );
        let err = ctx.assemble().expect_err("缺机密应报错");
        assert_eq!(
            err,
            AssembleError::MissingSecret(vec!["DEPLOY_KEY".into()])
        );
        assert!(err.to_string().contains("DEPLOY_KEY"), "detail 记名");
    }

    #[test]
    fn branch_falls_back_to_project_default_when_trigger_omits() {
        let mut ctx = input(
            TriggerDetail {
                by: "alice".into(),
                branch: None,
                commit: None,
                revision: None,
                params: vec![],
            },
            HashMap::new(),
            HashMap::new(),
        );
        ctx.set_secret("DEPLOY_KEY", b"v");
        let spec = ctx.assemble().expect("组装");
        assert_eq!(spec.scm.branch, "main", "缺省分支回退项目默认分支");
        assert!(spec.scm.commit.is_empty());
        // 步骤级 when `${SISY_BRANCH} == "main"` 通过（main），dev 步骤仍不发。
        assert_eq!(spec.steps.len(), 2);
    }

    #[test]
    fn svn_context_uses_revision() {
        let mut ctx = TestContext::base();
        ctx.project.scm_type = ScmType::Svn;
        ctx.project.default_branch = None;
        ctx.trigger = TriggerDetail {
            by: "alice".into(),
            branch: None,
            commit: None,
            revision: Some("42".into()),
            params: vec![],
        };
        ctx.set_secret("DEPLOY_KEY", b"v");
        let spec = ctx.assemble().expect("组装");
        assert_eq!(spec.scm.vcs, Vcs::Svn);
        assert_eq!(spec.scm.revision, "42");
        assert!(spec.scm.branch.is_empty());
        assert!(spec.scm.commit.is_empty());
    }

    #[test]
    fn merge_env_job_overrides_pipeline() {
        let pipeline = vec![
            EnvVar { name: "A".into(), value: "p".into() },
            EnvVar { name: "B".into(), value: "p".into() },
        ];
        let job = vec![EnvVar { name: "B".into(), value: "j".into() }];
        let merged = merge_env(pipeline, job);
        let map: HashMap<&str, &str> = merged
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(map["A"], "p");
        assert_eq!(map["B"], "j", "任务级覆盖 pipeline 级同名项");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn spec_round_trips_through_json() {
        let mut ctx = input(
            TriggerDetail {
                by: "alice".into(),
                branch: Some("main".into()),
                commit: None,
                revision: None,
                params: vec![],
            },
            HashMap::new(),
            HashMap::new(),
        );
        ctx.set_secret("DEPLOY_KEY", b"v");
        let spec = ctx.assemble().expect("组装");
        let json = serde_json::to_string(&spec).expect("序列化");
        let back: ResolvedJobSpec = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(spec, back, "spec 快照落库读回等价");
    }
}

//! Pipeline 定义 repo（票 B2a-T4；ADR-0006 修订版本语义）。
//!
//! 保存路径：输入先过 sisyphus-model 校验（单一事实源，schema 不解析定义
//! 内部）→ 事务内条件更新——按读到的当前 revision 做 `WHERE revision = ?`
//! 的 UPDATE，未命中即并发写冲突，回滚重试；首存 INSERT revision=1。
//! 由此并发下 revision 单调不回退、每次保存 +1。操作人为认证中间件注入的
//! 登录用户名（票 B2b-T1 起）。

use sisyphus_model::pipeline::{Pipeline, Revision};
use sqlx::SqlitePool;

use super::{StoreError, is_busy, is_unique_violation, now_ms};

/// 条件更新冲突的最大重试次数（超出视为持续写竞争，向上报错）。
const MAX_SAVE_ATTEMPTS: usize = 16;

/// 读回的 pipeline 定义：定义原文 + 修订版本语义字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPipeline {
    /// 定义原文（model serde JSON 文本，与提交等价读回）。
    pub definition: String,
    /// 当前修订版本号（从 1 起）。
    pub revision: u32,
    /// 最后保存的操作人（auth 落地前为占位标识）。
    pub operator: String,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后保存时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// Pipeline 定义 repo：保存（校验 + 事务内条件更新）与读取。
#[derive(Debug, Clone)]
pub struct PipelineRepo {
    pool: SqlitePool,
}

impl PipelineRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 保存定义：model 校验通过后按 Revision 语义递增落库，返回新修订版本。
    ///
    /// 项目不存在返回 [`StoreError::NotFound`]；校验失败返回
    /// [`StoreError::InvalidDefinition`]（错误清单整组透传）。
    pub async fn save(
        &self,
        project: &str,
        pipeline_name: &str,
        pipeline: &Pipeline,
        operator: &str,
    ) -> Result<Revision, StoreError> {
        if let Err(errors) = sisyphus_model::validate::validate(pipeline) {
            return Err(StoreError::InvalidDefinition(errors));
        }
        let project_id = self.resolve_project_id(project).await?;
        let definition = serde_json::to_string(pipeline).map_err(StoreError::DefinitionJson)?;

        // 事务内条件更新：并发保存未命中即重试，revision 单调不回退。
        let mut last_conflict = None;
        for _ in 0..MAX_SAVE_ATTEMPTS {
            match self
                .save_once(project_id, pipeline_name, &definition, operator)
                .await
            {
                Ok(revision) => return Ok(revision),
                Err(StoreError::Conflict(e)) => last_conflict = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::Conflict(
            last_conflict.unwrap_or_else(|| "并发保存冲突重试耗尽".into()),
        ))
    }

    /// 读当前定义；pipeline 不存在返回 `None`（项目不存在同样视为 `None`，
    /// 资源寻径差异由 API 层裁决）。
    pub async fn get(
        &self,
        project: &str,
        pipeline_name: &str,
    ) -> Result<Option<StoredPipeline>, StoreError> {
        let row = sqlx::query_as::<_, (String, i64, String, i64, i64)>(
            "SELECT p.definition, p.revision, p.operator, p.created_at, p.updated_at
             FROM pipelines p JOIN projects j ON j.id = p.project_id
             WHERE j.name = ? AND p.name = ?",
        )
        .bind(project)
        .bind(pipeline_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(definition, revision, operator, created_at, updated_at)| StoredPipeline {
                definition,
                revision: u32::try_from(revision)
                    .expect("schema 约束 revision 为非负且远小于 u32 上限"),
                operator,
                created_at,
                updated_at,
            },
        ))
    }

    /// 单次保存尝试：首存 INSERT（revision=1），续存条件 UPDATE（revision+1）。
    /// 数据库层的写竞争（UNIQUE 竞态、BUSY）折算为 Conflict 交外层重试。
    async fn save_once(
        &self,
        project_id: i64,
        pipeline_name: &str,
        definition: &str,
        operator: &str,
    ) -> Result<Revision, StoreError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;

        let current: Option<(i64, i64)> =
            sqlx::query_as("SELECT id, revision FROM pipelines WHERE project_id = ? AND name = ?")
                .bind(project_id)
                .bind(pipeline_name)
                .fetch_optional(&mut *tx)
                .await?;

        let (new_number, result) = match current {
            None => {
                let result = sqlx::query(
                    "INSERT INTO pipelines
                        (project_id, name, definition, revision, operator, created_at, updated_at)
                     VALUES (?, ?, ?, 1, ?, ?, ?)",
                )
                .bind(project_id)
                .bind(pipeline_name)
                .bind(definition)
                .bind(operator)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await;
                (1u32, result)
            }
            Some((id, revision)) => {
                let next = revision + 1;
                let result = sqlx::query(
                    "UPDATE pipelines
                     SET definition = ?, revision = ?, operator = ?, updated_at = ?
                     WHERE id = ? AND revision = ?",
                )
                .bind(definition)
                .bind(next)
                .bind(operator)
                .bind(now)
                .bind(id)
                .bind(revision)
                .execute(&mut *tx)
                .await;
                (
                    u32::try_from(next).expect("revision 为非负且远小于 u32 上限"),
                    result,
                )
            }
        };

        let result = match result {
            Ok(result) => result,
            // 两个并发首存撞 (project_id, name) 唯一键：一个落定，另一个重试
            // 走 UPDATE 分支——语义是 upsert，不是客户端冲突。
            Err(e) if is_unique_violation(&e) || is_busy(&e) => {
                tx.rollback().await?;
                return Err(StoreError::Conflict("并发首存撞唯一键".into()));
            }
            Err(e) => return Err(e.into()),
        };

        // 条件更新未命中：读到的 revision 已被并发保存推进，回滚重读。
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(StoreError::Conflict("条件更新未命中".into()));
        }

        tx.commit().await?;
        Ok(Revision {
            number: new_number,
            operator: operator.to_string(),
            at_ms: now,
        })
    }

    /// 解析项目名为行 id；不存在返回 [`StoreError::NotFound`]。
    async fn resolve_project_id(&self, project: &str) -> Result<i64, StoreError> {
        sqlx::query_scalar("SELECT id FROM projects WHERE name = ?")
            .bind(project)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("项目 {project} 不存在")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_model::pipeline::{
        CacheSpec, EnvVar, ExecutionEnv, Job, Parameter, ParameterType, ParameterValue, Shell,
        Stage, Step,
    };

    /// 夹具：已迁移库 + 预置项目 demo，返回 pipeline repo。
    async fn fixture() -> (tempfile::TempDir, PipelineRepo) {
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
        super::super::projects::ProjectRepo::new(pool.clone())
            .create(super::super::projects::NewProject {
                name: "demo".into(),
                scm_type: super::super::projects::ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: Some("main".into()),
            })
            .await
            .expect("建项目");
        (dir, PipelineRepo::new(pool))
    }

    fn minimal_pipeline() -> Pipeline {
        Pipeline {
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
                        command: "cargo build --release".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        }
    }

    /// 全特性定义：覆盖枚举 tagged 序列化、env、缓存、产物、机密等各字段
    /// 形态，serde 往返等价用它才有分量。
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
            notification: Some(sisyphus_model::pipeline::Notification { on_success: true }),
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
                    artifact_uploads: vec![sisyphus_model::pipeline::ArtifactUpload {
                        name: "bin".into(),
                        path: "target/release/sisyphus".into(),
                    }],
                    artifact_downloads: vec![],
                    caches: vec![CacheSpec {
                        key: "cargo-${SISY_BRANCH}".into(),
                        paths: vec![".cargo".into()],
                        files: vec!["Cargo.lock".into()],
                    }],
                    secrets: vec!["DOCKER_TOKEN".into()],
                    steps: vec![
                        Step::Checkout {
                            submodules: true,
                            when: None,
                        },
                        Step::Shell {
                            command: "cargo build --release".into(),
                            shell: Some(Shell::Bash),
                            when: None,
                        },
                    ],
                }],
            }],
            revision: None,
        }
    }

    #[tokio::test]
    async fn save_rejects_invalid_definition_with_full_error_list() {
        let (_dir, repo) = fixture().await;
        let mut pipeline = minimal_pipeline();
        pipeline.parameters.push(Parameter {
            name: "target".into(),
            r#type: ParameterType::String,
            required: true,
            default: None,
            description: None,
            choices: vec![],
        });
        pipeline.stages[0].when = Some("${SISY_WORKSPACE} == \"/x\"".into());

        let err = repo
            .save("demo", "build", &pipeline, "tester")
            .await
            .expect_err("应拒保存");
        let StoreError::InvalidDefinition(errors) = err else {
            panic!("应为校验失败：{err}");
        };
        // 整组透传：两处错误都在（不短路）。
        assert!(errors.len() >= 2, "错误清单：{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("必填参数必须带默认值"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("禁用 ${SISY_WORKSPACE}"))
        );
    }

    #[tokio::test]
    async fn save_to_missing_project_is_not_found() {
        let (_dir, repo) = fixture().await;
        let err = repo
            .save("nope", "build", &minimal_pipeline(), "tester")
            .await
            .expect_err("项目不存在应报错");
        assert!(
            matches!(err, StoreError::NotFound(_)),
            "应为 NotFound：{err}"
        );
    }

    #[tokio::test]
    async fn first_save_is_revision_1_then_increments_monotonically() {
        let (_dir, repo) = fixture().await;

        let r1 = repo
            .save("demo", "build", &minimal_pipeline(), "tester")
            .await
            .expect("首存");
        assert_eq!(r1.number, 1);
        assert_eq!(r1.operator, "tester", "操作人为调用侧传入的实名");
        assert!(r1.at_ms > 0);

        let r2 = repo
            .save("demo", "build", &minimal_pipeline(), "tester")
            .await
            .expect("续存");
        assert_eq!(r2.number, 2);
        let r3 = repo
            .save("demo", "build", &minimal_pipeline(), "tester")
            .await
            .expect("再存");
        assert_eq!(r3.number, 3);

        // 读回：当前定义 + revision + 操作人/时间。
        let stored = repo
            .get("demo", "build")
            .await
            .expect("读取")
            .expect("应存在");
        assert_eq!(stored.revision, 3);
        assert_eq!(stored.operator, "tester");
        assert!(stored.updated_at >= stored.created_at);
        let back: Pipeline = serde_json::from_str(&stored.definition).expect("定义应可解析");
        assert_eq!(back, minimal_pipeline(), "读回定义与提交等价");

        // 不存在：None。
        assert!(repo.get("demo", "nope").await.expect("读取").is_none());
        assert!(repo.get("nope", "build").await.expect("读取").is_none());
    }

    #[tokio::test]
    async fn full_featured_definition_round_trips_model_equivalent() {
        let (_dir, repo) = fixture().await;
        let submitted = full_pipeline();

        repo.save("demo", "release", &submitted, "tester")
            .await
            .expect("保存");
        let stored = repo
            .get("demo", "release")
            .await
            .expect("读取")
            .expect("应存在");
        let back: Pipeline = serde_json::from_str(&stored.definition).expect("解析");
        assert_eq!(back, submitted, "全特性定义落库读回与 model 类型等价");
    }

    /// 并发保存：revision 单调不回退（票 B2a-T4 AC）。真并发走多线程运行时，
    /// 条件更新保证每次保存拿到互不相同的递增号，终态 revision == 保存次数。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_saves_keep_revision_monotonic() {
        let (_dir, repo) = fixture().await;
        const N: usize = 12;

        let mut handles = Vec::new();
        for i in 0..N {
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                let mut pipeline = minimal_pipeline();
                pipeline.name = format!("build-{i}");
                repo.save("demo", "build", &pipeline, "tester")
                    .await
                    .expect("并发保存应成功")
            }));
        }
        let mut revisions: Vec<u32> = Vec::new();
        for h in handles {
            revisions.push(h.await.expect("join").number);
        }

        // 每次保存占号互不相同，且恰为 1..=N：无丢失、无回退、无重复。
        revisions.sort_unstable();
        let expect: Vec<u32> = (1..=N as u32).collect();
        assert_eq!(revisions, expect, "并发保存的 revision 应恰好 1..={N}");

        let stored = repo
            .get("demo", "build")
            .await
            .expect("读取")
            .expect("应存在");
        assert_eq!(stored.revision, N as u32, "终态 revision 应等于保存次数");
    }
}

//! 触发器 repo（票 B2c-T1，Spec B2c §2，ADR-0016）：cron/poll 触发源落库面。
//!
//! manual 触发不建行（触发即构建，上下文在 builds.trigger_detail）；cron 与
//! poll 各占一行，`UNIQUE(project_id, pipeline_name, kind)` 保证同 pipeline
//! 每类至多一个。spec 为 JSON 文本（cron 表达式 / poll 节奏）——schema 不
//! 解析内部，语义由 trigger 模块裁决。
//!
//! poll 基线（ADR-0016）：创建/启用时记录当前 head 作基线、不触发构建——
//! 只对之后的新提交触发，commit-id 去重。探测失败记入 last_probe_error、
//! 继续按节奏重试、不自动禁用（[`Self::record_probe`]）。触发历史以构建的
//! trigger_detail + builds 行呈现，不单独建表（Spec B2c §2）。

use sqlx::SqlitePool;

use super::{StoreError, is_unique_violation, now_ms};

/// 触发器行（`spec` 为 JSON 文本——cron 表达式或 poll 节奏的容器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRow {
    /// 行 id。
    pub id: i64,
    /// 属主项目 id。
    pub project_id: i64,
    /// pipeline 名。
    pub pipeline_name: String,
    /// 触发源类型（cron/poll；manual 不建行）。
    pub kind: TriggerKind,
    /// 配置 JSON（cron 表达式或 poll 节奏）。
    pub spec: String,
    /// 启停。
    pub enabled: bool,
    /// poll 基线 commit（创建/启用时记录、不触发；之后只对新提交触发）。
    pub baseline_commit: Option<String>,
    /// 最近探测时间（Unix 毫秒）。
    pub last_probe_at: Option<i64>,
    /// 最近探测错误（失败记入、继续按节奏重试）。
    pub last_probe_error: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 触发器类型（ADR-0016：cron 按表达式扫表；poll 按项目节奏轮询）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// cron 定时触发（表达式由 trigger 模块解析）。
    Cron,
    /// poll SCM 轮询（基线 + commit-id 去重）。
    Poll,
}

impl TriggerKind {
    /// 落库文本（schema 取值域）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Poll => "poll",
        }
    }

    /// 从落库文本解析（未知值视为库损坏）。
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "cron" => Ok(Self::Cron),
            "poll" => Ok(Self::Poll),
            other => Err(StoreError::Db(sqlx::Error::ColumnDecode {
                index: "triggers.kind".into(),
                source: format!("未知 triggers.kind：{other}").into(),
            })),
        }
    }
}

/// 新建/更新触发器输入（PUT/POST 语义由调用侧定；本 repo 只落行）。
#[derive(Debug, Clone)]
pub struct TriggerInput {
    /// 属主项目 id。
    pub project_id: i64,
    /// pipeline 名。
    pub pipeline_name: String,
    /// 触发源类型。
    pub kind: TriggerKind,
    /// 配置 JSON 文本。
    pub spec: String,
    /// 初始启停。
    pub enabled: bool,
}

/// 触发器 repo：建 / 启停与改配置 / 基线 / 探测历史 / 扫表。
#[derive(Debug, Clone)]
pub struct TriggerRepo {
    pool: SqlitePool,
}

impl TriggerRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 建触发器。poll 触发器创建即记基线但不触发（ADR-0016：配置变更≠
    /// 代码变更），基线由调用侧探测后经 [`Self::record_baseline`] 写入。
    /// 同 (project, pipeline, kind) 已存在返回 [`StoreError::Unique`]。
    pub async fn create(&self, input: TriggerInput) -> Result<TriggerRow, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO triggers
                (project_id, pipeline_name, kind, spec, enabled, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(input.project_id)
        .bind(&input.pipeline_name)
        .bind(input.kind.as_str())
        .bind(&input.spec)
        .bind(input.enabled)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) => {
                return Err(StoreError::Unique(format!(
                    "触发器已存在：{}:{}",
                    input.pipeline_name, input.kind.as_str(),
                )));
            }
            Err(e) => return Err(e.into()),
        };
        self.get(result.last_insert_rowid())
            .await
            .map(|row| row.expect("刚插入的行必存在"))
    }

    /// 改配置与启停（PATCH 语义：调用侧传目标值，None 表示不动）。
    /// 返回 false 表示触发器不存在。
    pub async fn update(
        &self,
        id: i64,
        spec: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<bool, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "UPDATE triggers SET spec = COALESCE(?, spec), enabled = COALESCE(?, enabled),
             updated_at = ? WHERE id = ?",
        )
        .bind(spec)
        .bind(enabled)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 记录 poll 基线（创建/启用时调用；ADR-0016 基线不触发）。也刷
    /// last_probe_at（本次探测时点）。返回 false 表示触发器不存在。
    pub async fn record_baseline(
        &self,
        id: i64,
        commit: &str,
        probed_at: i64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE triggers SET baseline_commit = ?, last_probe_at = ?,
             last_probe_error = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(commit)
        .bind(probed_at)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 记录一次探测结果：成功（`error` 为 None）清空历史错误、失败记入
    /// （继续按节奏重试、不自动禁用——ADR-0016）。返回 false 表示触发器
    /// 不存在。
    pub async fn record_probe(
        &self,
        id: i64,
        probed_at: i64,
        error: Option<&str>,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE triggers SET last_probe_at = ?, last_probe_error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(probed_at)
        .bind(error)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 某项目 pipeline 的触发器清单（cron/poll 各一；按类型排序输出稳定）。
    pub async fn list_by_pipeline(
        &self,
        project_id: i64,
        pipeline_name: &str,
    ) -> Result<Vec<TriggerRow>, StoreError> {
        let rows = sqlx::query_as::<_, TriggerTuple>(
            "SELECT id, project_id, pipeline_name, kind, spec, enabled, baseline_commit,
                    last_probe_at, last_probe_error, created_at, updated_at
             FROM triggers WHERE project_id = ? AND pipeline_name = ?
             ORDER BY kind",
        )
        .bind(project_id)
        .bind(pipeline_name)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TriggerRow::from_tuple).collect()
    }

    /// 启用的触发器全集（trigger 模块扫表：cron 按表达式匹配当前时刻、
    /// poll 按项目节奏轮询）。
    pub async fn list_enabled(&self) -> Result<Vec<TriggerRow>, StoreError> {
        let rows = sqlx::query_as::<_, TriggerTuple>(
            "SELECT id, project_id, pipeline_name, kind, spec, enabled, baseline_commit,
                    last_probe_at, last_probe_error, created_at, updated_at
             FROM triggers WHERE enabled = 1 ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TriggerRow::from_tuple).collect()
    }

    /// 按行 id 取触发器；不存在返回 `None`。
    pub async fn get(&self, id: i64) -> Result<Option<TriggerRow>, StoreError> {
        let row = sqlx::query_as::<_, TriggerTuple>(
            "SELECT id, project_id, pipeline_name, kind, spec, enabled, baseline_commit,
                    last_probe_at, last_probe_error, created_at, updated_at
             FROM triggers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TriggerRow::from_tuple).transpose()
    }

    /// 按 (project, pipeline, kind) 取触发器；不存在返回 `None`（REST 寻径）。
    pub async fn get_by_key(
        &self,
        project_id: i64,
        pipeline_name: &str,
        kind: TriggerKind,
    ) -> Result<Option<TriggerRow>, StoreError> {
        let row = sqlx::query_as::<_, TriggerTuple>(
            "SELECT id, project_id, pipeline_name, kind, spec, enabled, baseline_commit,
                    last_probe_at, last_probe_error, created_at, updated_at
             FROM triggers
             WHERE project_id = ? AND pipeline_name = ? AND kind = ?",
        )
        .bind(project_id)
        .bind(pipeline_name)
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(TriggerRow::from_tuple).transpose()
    }
}

/// triggers 行元组（列形态唯一收敛点，免逐查询散落 `Row::get`）。
type TriggerTuple = (
    i64,           // id
    i64,           // project_id
    String,        // pipeline_name
    String,        // kind
    String,        // spec
    bool,          // enabled
    Option<String>, // baseline_commit
    Option<i64>,   // last_probe_at
    Option<String>, // last_probe_error
    i64,           // created_at
    i64,           // updated_at
);

impl TriggerRow {
    /// 手工行映射（未知状态取值视为库损坏，与 ScmType 同纪律）。
    fn from_tuple(row: TriggerTuple) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            project_id: row.1,
            pipeline_name: row.2,
            kind: TriggerKind::parse(&row.3)?,
            spec: row.4,
            enabled: row.5,
            baseline_commit: row.6,
            last_probe_at: row.7,
            last_probe_error: row.8,
            created_at: row.9,
            updated_at: row.10,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};

    /// 独立临时目录 + 已迁移库 + 预置项目（store 缝测试形态）。
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

    fn input(project_id: i64, kind: TriggerKind) -> TriggerInput {
        TriggerInput {
            project_id,
            pipeline_name: "release".into(),
            kind,
            spec: match kind {
                TriggerKind::Cron => r#"{"expr":"0 2 * * *"}"#.into(),
                TriggerKind::Poll => r#"{"interval_minutes":5}"#.into(),
            },
            enabled: true,
        }
    }

    #[tokio::test]
    async fn create_list_get_and_unique_per_kind() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = TriggerRepo::new(pool.clone());

        let cron = repo
            .create(input(project_id, TriggerKind::Cron))
            .await
            .expect("建 cron");
        assert!(cron.id > 0);
        assert_eq!(cron.kind, TriggerKind::Cron);
        assert!(cron.enabled);
        assert_eq!(cron.baseline_commit, None, "基线待首次探测写入");
        assert!(cron.last_probe_at.is_none());
        assert!(cron.last_probe_error.is_none());

        // cron + poll 各一（同 pipeline 两类可共存）。
        let poll = repo
            .create(input(project_id, TriggerKind::Poll))
            .await
            .expect("建 poll");
        assert_eq!(poll.kind, TriggerKind::Poll);

        // 同 (project, pipeline, kind) 不可再建。
        let err = repo
            .create(input(project_id, TriggerKind::Cron))
            .await
            .expect_err("同类重复应拒绝");
        assert!(matches!(err, StoreError::Unique(_)));

        // 另一 pipeline：从零再来。
        let other = repo
            .create(TriggerInput {
                pipeline_name: "lint".into(),
                ..input(project_id, TriggerKind::Cron)
            })
            .await
            .expect("他 pipeline");
        assert_eq!(other.pipeline_name, "lint");

        // 按 key 寻径 + 按 id 读回等价。
        let by_key = repo
            .get_by_key(project_id, "release", TriggerKind::Cron)
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(by_key.id, cron.id);
        assert_eq!(
            repo.get(cron.id).await.expect("查").expect("应存在"),
            by_key
        );

        // 清单只含本 pipeline 的两类。
        let list = repo
            .list_by_pipeline(project_id, "release")
            .await
            .expect("清单");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, TriggerKind::Cron, "按 kind 排序稳定");
        assert!(repo
            .get_by_key(project_id, "release", TriggerKind::Cron)
            .await
            .expect("查")
            .is_some());
    }

    #[tokio::test]
    async fn update_changes_spec_and_enabled_selectively() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = TriggerRepo::new(pool.clone());
        let cron = repo
            .create(input(project_id, TriggerKind::Cron))
            .await
            .expect("建");

        // 只改配置（enabled None 不动）。
        assert!(repo
            .update(cron.id, Some(r#"{"expr":"0 6 * * 1-5"}"#), None)
            .await
            .expect("改配置"));
        let row = repo.get(cron.id).await.expect("查").expect("应存在");
        assert!(row.spec.contains("0 6 * * 1-5"));
        assert!(row.enabled, "enabled 未动");

        // 只改启停（spec None 不动）。
        assert!(repo
            .update(cron.id, None, Some(false))
            .await
            .expect("停用"));
        let row = repo.get(cron.id).await.expect("查").expect("应存在");
        assert!(!row.enabled);
        assert!(row.spec.contains("0 6 * * 1-5"), "spec 未动");

        // 不存在：false。
        assert!(!repo.update(cron.id + 999, None, None).await.expect("更新"));
    }

    /// 票 #45 AC：触发器基线/去重/历史（baseline_commit、last_probe 等）
    /// 可经 repo 读写。
    #[tokio::test]
    async fn baseline_and_probe_history_round_trip() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = TriggerRepo::new(pool.clone());
        let poll = repo
            .create(input(project_id, TriggerKind::Poll))
            .await
            .expect("建 poll");

        // 基线：创建/启用时探测当前 head 写入，不触发（本缝只落库）。
        assert!(repo
            .record_baseline(poll.id, "abc123", 1_000)
            .await
            .expect("记基线"));
        let row = repo.get(poll.id).await.expect("查").expect("应存在");
        assert_eq!(row.baseline_commit.as_deref(), Some("abc123"));
        assert_eq!(row.last_probe_at, Some(1_000));
        assert_eq!(row.last_probe_error, None, "记基线清历史错误");

        // 探测成功：清错误（baseline 不动——基线一旦记录持续生效）。
        assert!(repo
            .record_probe(poll.id, 2_000, None)
            .await
            .expect("探测成功"));
        let row = repo.get(poll.id).await.expect("查").expect("应存在");
        assert_eq!(row.last_probe_at, Some(2_000));
        assert_eq!(row.baseline_commit.as_deref(), Some("abc123"));
        assert!(row.last_probe_error.is_none());

        // 探测失败：错误记入、继续可探测（不自动禁用、不静默停摆）。
        assert!(repo
            .record_probe(poll.id, 3_000, Some("git ls-remote failed"))
            .await
            .expect("探测失败"));
        let row = repo.get(poll.id).await.expect("查").expect("应存在");
        assert_eq!(row.last_probe_at, Some(3_000));
        assert_eq!(row.last_probe_error.as_deref(), Some("git ls-remote failed"));
        assert!(row.enabled, "探测失败不自动禁用");
        assert_eq!(row.baseline_commit.as_deref(), Some("abc123"), "基线不受影响");

        // 再次成功：错误清空（历史即「最近一次」——完整历史随构建
        // trigger_detail + builds 行呈现，Spec B2c §2 不单独建表）。
        assert!(repo
            .record_probe(poll.id, 4_000, None)
            .await
            .expect("再探测"));
        let row = repo.get(poll.id).await.expect("查").expect("应存在");
        assert!(row.last_probe_error.is_none());
    }

    #[tokio::test]
    async fn list_enabled_scans_only_enabled_rows() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = TriggerRepo::new(pool.clone());
        let cron = repo
            .create(input(project_id, TriggerKind::Cron))
            .await
            .expect("建 cron");
        let poll = repo
            .create(input(project_id, TriggerKind::Poll))
            .await
            .expect("建 poll");
        let other = repo
            .create(TriggerInput {
                pipeline_name: "nightly".into(),
                ..input(project_id, TriggerKind::Cron)
            })
            .await
            .expect("另一 pipeline");

        assert_eq!(repo.list_enabled().await.expect("扫表").len(), 3);

        // 停用 cron 后：扫表不含停用行（trigger 模块只消费启用的）。
        repo.update(cron.id, None, Some(false))
            .await
            .expect("停用 cron");
        let enabled = repo.list_enabled().await.expect("扫表");
        let ids: Vec<i64> = enabled.iter().map(|t| t.id).collect();
        assert!(!ids.contains(&cron.id), "停用行不在扫表");
        assert!(ids.contains(&poll.id) && ids.contains(&other.id));
    }
}

//! 项目元数据 repo（票 B2a-T4；CONTEXT.md「项目」词条）。
//!
//! 项目元数据读写：list / create / get / update；删除及其级联语义
//! （pipeline 删除对构建历史的影响）由删除 repo 单独承载。

use sqlx::SqlitePool;

use super::{StoreError, is_unique_violation, now_ms};

/// 项目绑定的仓库类型（git/svn，或不绑定 SCM 的空工作区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmType {
    /// git 仓库（默认分支可空）。
    Git,
    /// svn 仓库（URL 即唯一监控对象）。
    Svn,
    /// 不绑定版本管理器的空工作区项目。
    None,
}

impl ScmType {
    /// 落库文本（schema CHECK 约束的取值域）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Svn => "svn",
            Self::None => "none",
        }
    }

    /// 从落库文本解析（schema 已约束取值域，未知值视为库损坏）。
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "git" => Ok(Self::Git),
            "svn" => Ok(Self::Svn),
            "none" => Ok(Self::None),
            other => Err(StoreError::Db(sqlx::Error::ColumnDecode {
                index: "scm_type".into(),
                source: format!("未知 scm_type：{other}").into(),
            })),
        }
    }
}

/// 新建项目输入（字段校验在 API 层，这里只管落库语义）。
#[derive(Debug, Clone)]
pub struct NewProject {
    /// 项目名（唯一键）。
    pub name: String,
    /// 仓库类型。
    pub scm_type: ScmType,
    /// 仓库 URL。
    pub scm_url: String,
    /// git 默认分支（可空；svn 不适用）。
    pub default_branch: Option<String>,
}

/// 项目设置 PATCH 输入。`default_branch: None` 表示字段缺省不变，
/// `Some(None)` 表示显式清除；`scm_url` 仅支持非空替换。
#[derive(Debug, Clone)]
pub struct UpdateProject {
    /// 新仓库 URL；`None` 表示缺省不变。
    pub scm_url: Option<String>,
    /// 默认分支三态值：缺省不变、`Some(None)` 清除、`Some(Some(v))` 替换。
    pub default_branch: Option<Option<String>>,
}

/// 项目行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// 行 id。
    pub id: i64,
    /// 项目名（唯一）。
    pub name: String,
    /// 仓库类型。
    pub scm_type: ScmType,
    /// 仓库 URL。
    pub scm_url: String,
    /// git 默认分支（可空；svn 不适用）。
    pub default_branch: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
    /// 项目下的流水线定义数量。
    pub pipeline_count: i64,
}

/// 项目元数据 repo：list / create / get / update。
#[derive(Debug, Clone)]
pub struct ProjectRepo {
    pool: SqlitePool,
}

impl ProjectRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 列出全部项目（按名排序，输出稳定便于测试与展示）。
    pub async fn list(&self) -> Result<Vec<Project>, StoreError> {
        let rows =
            sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64, i64)>(
                "SELECT p.id, p.name, p.scm_type, p.scm_url, p.default_branch,
                    p.created_at, p.updated_at, COUNT(pl.id)
             FROM projects p
             LEFT JOIN pipelines pl ON pl.project_id = p.id
             WHERE p.lifecycle = 'active'
             GROUP BY p.id
             ORDER BY p.name",
            )
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(Project::from_row).collect()
    }

    /// 按可见性列项目（票 B2b-T5）：全局 admin 全量；普通用户只列自己有
    /// 角色的项目（无角色项目不可见，单查亦 404——存在性不外泄）。
    /// 按名排序与 [`Self::list`] 一致。
    pub async fn list_visible(
        &self,
        is_admin: bool,
        user_id: i64,
    ) -> Result<Vec<Project>, StoreError> {
        if is_admin {
            return self.list().await;
        }
        self.list_for_member(user_id, None).await
    }

    /// 列出调用者可管理的项目：全局 admin 隐含管理全部项目；普通用户仅列
    /// 显式角色为 `admin` 的项目。按名排序与其他项目清单一致。
    pub async fn list_manageable(
        &self,
        is_admin: bool,
        user_id: i64,
    ) -> Result<Vec<Project>, StoreError> {
        if is_admin {
            return self.list().await;
        }
        self.list_for_member(user_id, Some("admin")).await
    }

    /// 按成员关系列项目；`required_role` 为空时接受任意显式角色。
    async fn list_for_member(
        &self,
        user_id: i64,
        required_role: Option<&str>,
    ) -> Result<Vec<Project>, StoreError> {
        let rows =
            sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64, i64)>(
                "SELECT p.id, p.name, p.scm_type, p.scm_url, p.default_branch,
                    p.created_at, p.updated_at, COUNT(pl.id)
             FROM projects p
             JOIN project_members m ON m.project_id = p.id
             LEFT JOIN pipelines pl ON pl.project_id = p.id
             WHERE p.lifecycle = 'active' AND m.user_id = ? AND (? IS NULL OR m.role = ?)
             GROUP BY p.id
             ORDER BY p.name",
            )
            .bind(user_id)
            .bind(required_role)
            .bind(required_role)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(Project::from_row).collect()
    }

    /// 创建项目；项目名已存在返回 [`StoreError::Unique`]。
    pub async fn create(&self, input: NewProject) -> Result<Project, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO projects (name, scm_type, scm_url, default_branch, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&input.name)
        .bind(input.scm_type.as_str())
        .bind(&input.scm_url)
        .bind(&input.default_branch)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;

        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) => {
                return Err(StoreError::Unique(format!("项目名已存在：{}", input.name)));
            }
            Err(e) => return Err(e.into()),
        };
        let id = result.last_insert_rowid();
        Ok(Project {
            id,
            name: input.name,
            scm_type: input.scm_type,
            scm_url: input.scm_url,
            default_branch: input.default_branch,
            created_at: now,
            updated_at: now,
            pipeline_count: 0,
        })
    }

    /// 按名更新项目设置，并重新读取完整项目行（含 pipeline_count 与
    /// 数据库生成的 updated_at），保证 REST 响应不是写入前的快照。
    pub async fn update(
        &self,
        id: i64,
        name: &str,
        input: UpdateProject,
    ) -> Result<Project, StoreError> {
        let now = now_ms();
        match (input.scm_url, input.default_branch) {
            (Some(scm_url), Some(default_branch)) => {
                sqlx::query(
                    "UPDATE projects SET scm_url = ?, default_branch = ?, updated_at = MAX(updated_at + 1, ?)
                     WHERE id = ? AND lifecycle = 'active'",
                )
                .bind(scm_url)
                .bind(default_branch)
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            (Some(scm_url), None) => {
                sqlx::query(
                    "UPDATE projects SET scm_url = ?, updated_at = MAX(updated_at + 1, ?)
                     WHERE id = ? AND lifecycle = 'active'",
                )
                .bind(scm_url)
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            (None, Some(default_branch)) => {
                sqlx::query(
                    "UPDATE projects SET default_branch = ?, updated_at = MAX(updated_at + 1, ?)
                     WHERE id = ? AND lifecycle = 'active'",
                )
                .bind(default_branch)
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            (None, None) => {}
        }
        self.get_by_name(name)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("项目 {name} 不存在")))
    }

    /// 按名取项目；不存在返回 `None`。
    pub async fn get_by_name(&self, name: &str) -> Result<Option<Project>, StoreError> {
        let row =
            sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64, i64)>(
                "SELECT p.id, p.name, p.scm_type, p.scm_url, p.default_branch,
                    p.created_at, p.updated_at, COUNT(pl.id)
             FROM projects p
             LEFT JOIN pipelines pl ON pl.project_id = p.id
             WHERE p.name = ? AND p.lifecycle = 'active'
             GROUP BY p.id",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Project::from_row).transpose()
    }

    /// 按行 id 取项目；不存在返回 `None`（engine 组装 SCM 上下文按
    /// builds.project_id 寻径）。
    pub async fn get_by_id(&self, id: i64) -> Result<Option<Project>, StoreError> {
        let row =
            sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64, i64)>(
                "SELECT p.id, p.name, p.scm_type, p.scm_url, p.default_branch,
                    p.created_at, p.updated_at, COUNT(pl.id)
             FROM projects p
             LEFT JOIN pipelines pl ON pl.project_id = p.id
             WHERE p.id = ? AND p.lifecycle = 'active'
             GROUP BY p.id",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Project::from_row).transpose()
    }
}

impl Project {
    /// 手工行映射（列形态唯一收敛点，免逐查询散落 `Row::get`）。
    fn from_row(
        row: (i64, String, String, String, Option<String>, i64, i64, i64),
    ) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            name: row.1,
            scm_type: ScmType::parse(&row.2)?,
            scm_url: row.3,
            default_branch: row.4,
            created_at: row.5,
            updated_at: row.6,
            pipeline_count: row.7,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录 + 临时 db 文件的已迁移库（store 缝测试形态，票 #32 沿用）。
    async fn migrated_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时目录");
        // 走生产序列：Config::load 建目录布局，bootstrap 开池+PRAGMA+迁移。
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = super::super::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        (dir, pool)
    }

    fn new_project(name: &str) -> NewProject {
        NewProject {
            name: name.into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
        }
    }

    #[tokio::test]
    async fn create_get_list_round_trip() {
        let (_dir, pool) = migrated_pool().await;
        let repo = ProjectRepo::new(pool.clone());

        let created = repo.create(new_project("demo")).await.expect("创建");
        assert!(created.id > 0);
        assert_eq!(created.scm_type, ScmType::Git);
        assert_eq!(created.default_branch.as_deref(), Some("main"));
        assert_eq!(created.pipeline_count, 0);
        assert!(created.created_at > 0 && created.updated_at == created.created_at);

        repo.create(NewProject {
            name: "svn-proj".into(),
            scm_type: ScmType::Svn,
            scm_url: "https://svn.example.com/trunk".into(),
            default_branch: None,
        })
        .await
        .expect("创建 svn 项目");

        // get：按名读回，字段等价。
        let got = repo
            .get_by_name("demo")
            .await
            .expect("读取")
            .expect("应存在");
        assert_eq!(got, created);

        // list：两个都在，按名排序。
        let all = repo.list().await.expect("清单");
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["demo", "svn-proj"]);
        assert_eq!(all[1].scm_type, ScmType::Svn);
        assert_eq!(all[1].default_branch, None);

        // 项目清单与单查都携带流水线定义数量，避免项目页额外拉全量流水线。
        sqlx::query(
            "INSERT INTO pipelines
                (project_id, name, definition, revision, operator, created_at, updated_at)
             VALUES (?, 'main', '{}', 1, 'admin', 1, 1)",
        )
        .bind(created.id)
        .execute(&pool)
        .await
        .expect("插入流水线");
        assert_eq!(
            repo.get_by_name("demo")
                .await
                .expect("读取")
                .expect("应存在")
                .pipeline_count,
            1
        );
        assert_eq!(repo.list().await.expect("清单")[0].pipeline_count, 1);

        // 不存在的名字：None 而非错误。
        assert!(repo.get_by_name("nope").await.expect("读取").is_none());
    }

    #[tokio::test]
    async fn duplicate_name_is_unique_error() {
        let (_dir, pool) = migrated_pool().await;
        let repo = ProjectRepo::new(pool);

        repo.create(new_project("demo")).await.expect("首建");
        let err = repo
            .create(new_project("demo"))
            .await
            .expect_err("重名应拒绝");
        assert!(matches!(err, StoreError::Unique(_)), "应为唯一冲突：{err}");
    }

    #[tokio::test]
    async fn empty_workspace_round_trips_without_scm() {
        let (_dir, pool) = migrated_pool().await;
        let repo = ProjectRepo::new(pool);
        let created = repo
            .create(NewProject {
                name: "artifact-only".into(),
                scm_type: ScmType::None,
                scm_url: String::new(),
                default_branch: None,
            })
            .await
            .expect("创建空工作区项目");

        assert_eq!(created.scm_type, ScmType::None);
        assert!(created.scm_url.is_empty());
        assert_eq!(
            repo.get_by_name("artifact-only")
                .await
                .expect("读取")
                .unwrap(),
            created
        );
    }
}

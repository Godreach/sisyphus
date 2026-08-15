//! 项目元数据 repo（票 B2a-T4；CONTEXT.md「项目」词条）。
//!
//! v1 只交付 list / create / get——update/delete 及其级联语义（pipeline 删除
//! 对构建历史的影响）归后续批次裁定，不预开方法面。

use sqlx::SqlitePool;

use super::{StoreError, is_unique_violation, now_ms};

/// 项目绑定的仓库类型（CONTEXT.md：git 带默认分支、svn 无分支概念）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmType {
    /// git 仓库（默认分支可空）。
    Git,
    /// svn 仓库（URL 即唯一监控对象）。
    Svn,
}

impl ScmType {
    /// 落库文本（schema CHECK 约束的取值域）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Svn => "svn",
        }
    }

    /// 从落库文本解析（schema 已约束取值域，未知值视为库损坏）。
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "git" => Ok(Self::Git),
            "svn" => Ok(Self::Svn),
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
}

/// 项目元数据 repo：list / create / get。
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
        let rows = sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64)>(
            "SELECT id, name, scm_type, scm_url, default_branch, created_at, updated_at
             FROM projects ORDER BY name",
        )
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
        })
    }

    /// 按名取项目；不存在返回 `None`。
    pub async fn get_by_name(&self, name: &str) -> Result<Option<Project>, StoreError> {
        let row = sqlx::query_as::<_, (i64, String, String, String, Option<String>, i64, i64)>(
            "SELECT id, name, scm_type, scm_url, default_branch, created_at, updated_at
             FROM projects WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Project::from_row).transpose()
    }
}

impl Project {
    /// 手工行映射（列形态唯一收敛点，免逐查询散落 `Row::get`）。
    fn from_row(
        row: (i64, String, String, String, Option<String>, i64, i64),
    ) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            name: row.1,
            scm_type: ScmType::parse(&row.2)?,
            scm_url: row.3,
            default_branch: row.4,
            created_at: row.5,
            updated_at: row.6,
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
        let repo = ProjectRepo::new(pool);

        let created = repo.create(new_project("demo")).await.expect("创建");
        assert!(created.id > 0);
        assert_eq!(created.scm_type, ScmType::Git);
        assert_eq!(created.default_branch.as_deref(), Some("main"));
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
}

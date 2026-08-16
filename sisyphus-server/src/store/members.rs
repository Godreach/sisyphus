//! 项目成员 repo（票 B2b-T5，ADR-0014）：项目 × 用户 → 三档角色。
//!
//! 角色读取（授权 extractor 逐请求查库，成员变更即时生效）与整组替换
//! （PUT 语义：以提交清单为准，事务内删旧插新）。全局 admin 不落本表
//! ——隐含权限由授权层裁决，repo 只管显式成员行。

use sqlx::SqlitePool;

use super::StoreError;
use crate::auth::Role;

/// 成员行（项目域视图：带用户名，成员管理端点直接消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    /// 用户 id。
    pub user_id: i64,
    /// 用户名。
    pub username: String,
    /// 项目角色。
    pub role: Role,
}

/// 项目成员 repo：角色读取 / 成员清单 / 整组替换 / 目录守卫判定。
#[derive(Debug, Clone)]
pub struct MemberRepo {
    pool: SqlitePool,
}

impl MemberRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 用户在某项目的角色；未分配返回 `None`（授权层裁决 404/403）。
    pub async fn role_of(&self, project_id: i64, user_id: i64) -> Result<Option<Role>, StoreError> {
        let row = sqlx::query_scalar::<_, String>(
            "SELECT role FROM project_members WHERE project_id = ? AND user_id = ?",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|s| Role::parse(&s)).transpose()
    }

    /// 项目成员清单（联用户名，按用户名排序输出稳定）。
    pub async fn list_by_project(&self, project_id: i64) -> Result<Vec<MemberRow>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT m.user_id, u.username, m.role
                 FROM project_members m JOIN users u ON u.id = m.user_id
                 WHERE m.project_id = ?
                 ORDER BY u.username",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(user_id, username, role)| {
                Ok(MemberRow {
                    user_id,
                    username,
                    role: Role::parse(&role)?,
                })
            })
            .collect()
    }

    /// 整组替换项目成员（PUT 语义）：事务内删全部旧行再插入提交清单，
    /// 中途失败整体回滚（不出现半套成员）。用户存在性由调用侧先行校验
    /// （repo 只管落库语义，引用完整性由外键兜底）。
    pub async fn replace_all(
        &self,
        project_id: i64,
        assignments: &[(i64, Role)],
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM project_members WHERE project_id = ?")
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        for (user_id, role) in assignments {
            sqlx::query("INSERT INTO project_members (project_id, user_id, role) VALUES (?, ?, ?)")
                .bind(project_id)
                .bind(user_id)
                .bind(role.as_str())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 用户是否为任意项目的 admin（用户目录端点的守卫：项目 admin 即可读
    /// 最小目录，全局 admin 由调用侧先行放行）。
    pub async fn is_any_project_admin(&self, user_id: i64) -> Result<bool, StoreError> {
        let hit: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM project_members WHERE user_id = ? AND role = 'admin' LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(hit.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::super::projects::{NewProject, ProjectRepo, ScmType};
    use super::super::users::UserRepo;
    use super::*;

    /// 独立临时目录 + 已迁移库（store 缝测试形态，沿用 projects/users）。
    async fn migrated_pool() -> (tempfile::TempDir, SqlitePool) {
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
        (dir, pool)
    }

    /// 夹具：两项目 + 三用户，返回成员 repo。
    async fn fixture() -> (tempfile::TempDir, MemberRepo, SqlitePool) {
        let (dir, pool) = migrated_pool().await;
        let projects = ProjectRepo::new(pool.clone());
        for name in ["alpha", "beta"] {
            projects
                .create(NewProject {
                    name: name.into(),
                    scm_type: ScmType::Git,
                    scm_url: "https://example.com/repo".into(),
                    default_branch: None,
                })
                .await
                .expect("建项目");
        }
        let users = UserRepo::new(pool.clone());
        for name in ["alice", "bob"] {
            users
                .create(name, "$argon2id$dummy", false)
                .await
                .expect("建用户");
        }
        let repo = MemberRepo::new(pool.clone());
        (dir, repo, pool)
    }

    async fn user_id(pool: &SqlitePool, username: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(pool)
            .await
            .expect("用户存在")
    }

    async fn project_id(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM projects WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .expect("项目存在")
    }

    #[tokio::test]
    async fn role_read_after_replace_and_absent_is_none() {
        let (_dir, repo, pool) = fixture().await;
        let (alpha, alice, bob) = (
            project_id(&pool, "alpha").await,
            user_id(&pool, "alice").await,
            user_id(&pool, "bob").await,
        );

        // 未分配：None。
        assert_eq!(repo.role_of(alpha, alice).await.expect("读取"), None);

        // 整组替换后可读回；未列入的 bob 仍 None。
        repo.replace_all(alpha, &[(alice, Role::Viewer)])
            .await
            .expect("替换");
        assert_eq!(
            repo.role_of(alpha, alice).await.expect("读取"),
            Some(Role::Viewer)
        );
        assert_eq!(repo.role_of(alpha, bob).await.expect("读取"), None);

        // 再替换：旧角色整体作废（升档 + 移除都即时可见）。
        repo.replace_all(alpha, &[(alice, Role::Admin), (bob, Role::Runner)])
            .await
            .expect("再替换");
        assert_eq!(
            repo.role_of(alpha, alice).await.expect("读取"),
            Some(Role::Admin)
        );
        assert_eq!(
            repo.role_of(alpha, bob).await.expect("读取"),
            Some(Role::Runner)
        );
    }

    #[tokio::test]
    async fn replace_is_scoped_to_project_and_lists_with_usernames() {
        let (_dir, repo, pool) = fixture().await;
        let (alpha, beta) = (
            project_id(&pool, "alpha").await,
            project_id(&pool, "beta").await,
        );
        let (alice, bob) = (user_id(&pool, "alice").await, user_id(&pool, "bob").await);

        repo.replace_all(alpha, &[(bob, Role::Viewer), (alice, Role::Admin)])
            .await
            .expect("替换 alpha");
        repo.replace_all(beta, &[(bob, Role::Runner)])
            .await
            .expect("替换 beta");

        // 清单联用户名、按用户名排序；项目间互不串。
        let members = repo.list_by_project(alpha).await.expect("清单");
        assert_eq!(
            members,
            vec![
                MemberRow {
                    user_id: alice,
                    username: "alice".into(),
                    role: Role::Admin,
                },
                MemberRow {
                    user_id: bob,
                    username: "bob".into(),
                    role: Role::Viewer,
                },
            ]
        );
        let members = repo.list_by_project(beta).await.expect("清单");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].role, Role::Runner);

        // 空清单：整组替换为空 = 移除全部成员。
        repo.replace_all(beta, &[]).await.expect("清空 beta");
        assert!(repo.list_by_project(beta).await.expect("清单").is_empty());
        // alpha 不受影响。
        assert_eq!(repo.list_by_project(alpha).await.expect("清单").len(), 2);
    }

    #[tokio::test]
    async fn any_project_admin_gate_reflects_membership() {
        let (_dir, repo, pool) = fixture().await;
        let (alpha, beta) = (
            project_id(&pool, "alpha").await,
            project_id(&pool, "beta").await,
        );
        let (alice, bob) = (user_id(&pool, "alice").await, user_id(&pool, "bob").await);

        // 无任何成员：双双 false。
        assert!(!repo.is_any_project_admin(alice).await.expect("判定"));
        assert!(!repo.is_any_project_admin(bob).await.expect("判定"));

        // 非 admin 角色（viewer）不开门；跨项目任一 admin 即开门。
        repo.replace_all(alpha, &[(alice, Role::Viewer)])
            .await
            .expect("alice viewer");
        assert!(!repo.is_any_project_admin(alice).await.expect("判定"));

        repo.replace_all(beta, &[(alice, Role::Admin)])
            .await
            .expect("alice beta admin");
        assert!(repo.is_any_project_admin(alice).await.expect("判定"));
        assert!(!repo.is_any_project_admin(bob).await.expect("判定"));
    }
}

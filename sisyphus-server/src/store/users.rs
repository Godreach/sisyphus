//! 用户 repo（票 B2b-T1，ADR-0014）。
//!
//! v1 面收敛为认证最小闭环所需：计数（setup wizard 空库判定）、创建、
//! 按名/按 id 读取。用户管理（建/禁/改密）与项目成员随后续批次扩面；
//! 只禁用不物理删除，本 repo 不提供删除方法。

use sqlx::SqlitePool;

use super::{StoreError, is_unique_violation, now_ms};

/// 用户行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// 行 id。
    pub id: i64,
    /// 用户名（唯一）。
    pub username: String,
    /// argon2id PHC 字符串（明文永不上库）。
    pub password_hash: String,
    /// 全局管理员（setup wizard 创建的首个用户即 true）。
    pub is_admin: bool,
    /// 禁用标志（禁用即时踢线由 session 删除兑现，本 repo 只管行状态）。
    pub disabled: bool,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 用户 repo：计数 / 创建 / 读取。
#[derive(Debug, Clone)]
pub struct UserRepo {
    pool: SqlitePool,
}

impl UserRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 用户数（setup wizard 的空库判定：0 = 允许创建首个全局 admin）。
    pub async fn count(&self) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// 创建用户；用户名已存在返回 [`StoreError::Unique`]。
    pub async fn create(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<User, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO users (username, password_hash, is_admin, disabled, created_at, updated_at)
             VALUES (?, ?, ?, 0, ?, ?)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(is_admin)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;

        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) => {
                return Err(StoreError::Unique(format!("用户名已存在：{username}")));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(User {
            id: result.last_insert_rowid(),
            username: username.to_string(),
            password_hash: password_hash.to_string(),
            is_admin,
            disabled: false,
            created_at: now,
            updated_at: now,
        })
    }

    /// 按用户名取用户；不存在返回 `None`。
    pub async fn get_by_username(&self, username: &str) -> Result<Option<User>, StoreError> {
        sqlx::query_as::<_, (i64, String, String, i64, i64, i64, i64)>(
            "SELECT id, username, password_hash, is_admin, disabled, created_at, updated_at
             FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?
        .map(User::from_row)
        .transpose()
    }

    /// 按 id 取用户（认证路径：session 行 → 用户行）；不存在返回 `None`。
    pub async fn get_by_id(&self, id: i64) -> Result<Option<User>, StoreError> {
        sqlx::query_as::<_, (i64, String, String, i64, i64, i64, i64)>(
            "SELECT id, username, password_hash, is_admin, disabled, created_at, updated_at
             FROM users WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(User::from_row)
        .transpose()
    }

    /// 最小用户目录（票 B2b-T5）：仅 id + 用户名，供成员分配下拉。排除
    /// 已禁用用户（无认证面，分配角色无意义）；密码哈希等列不出库。
    pub async fn list_directory(&self) -> Result<Vec<(i64, String)>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, username FROM users WHERE disabled = 0 ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

impl User {
    /// 手工行映射（布尔列 0/1 收敛点）。
    fn from_row(row: (i64, String, String, i64, i64, i64, i64)) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            username: row.1,
            password_hash: row.2,
            is_admin: row.3 != 0,
            disabled: row.4 != 0,
            created_at: row.5,
            updated_at: row.6,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录 + 已迁移库（store 缝测试形态，沿用 projects/pipelines）。
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

    #[tokio::test]
    async fn create_get_round_trip_and_no_plaintext_in_db() {
        let (_dir, pool) = migrated_pool().await;
        let repo = UserRepo::new(pool.clone());

        assert_eq!(repo.count().await.expect("空库计数"), 0, "空库应为 0");

        let phc = crate::auth::hash_password("correct horse").await;
        let created = repo
            .create("root", &phc, true)
            .await
            .expect("创建首个 admin");
        assert!(created.id > 0);
        assert!(created.is_admin);
        assert!(!created.disabled);

        // store 缝直查临时库：库里是 argon2id PHC，明文不落库（票 B2b-T1 AC）。
        let stored: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE username = 'root'")
                .fetch_one(&pool)
                .await
                .expect("直查 password_hash");
        assert!(stored.starts_with("$argon2id$"), "PHC 形态：{stored}");
        assert_ne!(stored, "correct horse");
        assert!(!stored.contains("correct horse"));

        // 读回等价；未知用户 None。
        let got = repo
            .get_by_username("root")
            .await
            .expect("按名读取")
            .expect("应存在");
        assert_eq!(got, created);
        let by_id = repo
            .get_by_id(created.id)
            .await
            .expect("按 id 读取")
            .expect("应存在");
        assert_eq!(by_id.username, "root");
        assert!(repo.get_by_username("nope").await.expect("读取").is_none());
    }

    #[tokio::test]
    async fn duplicate_username_is_unique_error() {
        let (_dir, pool) = migrated_pool().await;
        let repo = UserRepo::new(pool);
        let phc = crate::auth::hash_password("password1").await;

        repo.create("root", &phc, true).await.expect("首建");
        let err = repo
            .create("root", &phc, false)
            .await
            .expect_err("重名应拒绝");
        assert!(matches!(err, StoreError::Unique(_)), "应为唯一冲突：{err}");
    }
}

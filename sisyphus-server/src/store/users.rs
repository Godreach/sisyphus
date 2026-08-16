//! 用户 repo（票 B2b-T1，ADR-0014）。
//!
//! 认证面：计数（setup wizard 空库判定）、创建、按名/按 id 读取。用户管理
//! 面（票 B2b-T4）：全量列表、禁用/启用（禁用同事务级联删该用户全部
//! session 与 PAT——同秒踢线）、密码覆写（管理员代办重置 / 自助改密的
//! 落库点）。只禁用不物理删除，本 repo 不提供删除方法；历史操作人字段
//! 因此永不悬空。

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

    /// 全量用户列表（票 B2b-T4，全局 admin 管理面）：含已禁用——只禁用不
    /// 物理删除，禁用行仍要可见可管理；按用户名排序。密码哈希由 API 层
    /// 裁剪，不出响应。
    pub async fn list(&self) -> Result<Vec<User>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String, String, i64, i64, i64, i64)>(
            "SELECT id, username, password_hash, is_admin, disabled, created_at, updated_at
             FROM users ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(User::from_row).collect()
    }

    /// 禁用 / 启用（票 B2b-T4，ADR-0014）：禁用在同一事务内级联删除该用户
    /// 全部 session 行与 PAT 行——旧 cookie 与旧令牌下一请求即 401，同秒
    /// 生效；启用只翻标志（禁用期凭据面已清空，用户以密码重新登录）。
    /// 用户行本身永不删除（历史操作人字段不悬空）。目标不存在返回
    /// `None`；幂等（重复禁用/启用同值无副作用差异）。
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<Option<User>, StoreError> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("UPDATE users SET disabled = ?, updated_at = ? WHERE id = ?")
            .bind(disabled)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            // 回滚而非 commit：空事务无写入差异，显式回滚语义更直白。
            tx.rollback().await?;
            return Ok(None);
        }
        if disabled {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM personal_access_tokens WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.get_by_id(id).await
    }

    /// 覆写密码哈希（票 B2b-T4：管理员代办重置 / 自助改密的落库面）。
    /// 只换哈希不动凭据面：既有 session 与 PAT 不受密码变更牵连（各自有
    /// 独立吊销途径）。目标不存在返回 `false`。
    pub async fn set_password(
        &self,
        id: i64,
        password_hash: &str,
    ) -> Result<bool, StoreError> {
        let result =
            sqlx::query("UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?")
                .bind(password_hash)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
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

    #[tokio::test]
    async fn list_returns_all_users_including_disabled_ordered_by_username() {
        let (_dir, pool) = migrated_pool().await;
        let repo = UserRepo::new(pool);
        let phc = crate::auth::hash_password("password1").await;

        let root = repo.create("root", &phc, true).await.expect("admin");
        let alice = repo.create("alice", &phc, false).await.expect("普通用户");
        repo.set_disabled(alice.id, true).await.expect("禁用 alice");

        let list = repo.list().await.expect("列表");
        assert_eq!(
            list.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(),
            vec!["alice", "root"],
            "按用户名排序，含已禁用"
        );
        assert!(list.iter().any(|u| u.id == alice.id && u.disabled), "禁用行仍可见");
        assert!(list.iter().any(|u| u.id == root.id));
    }

    /// 票 B2b-T4 AC：禁用即时删除该用户全部 session 与 PAT（同事务），
    /// 用户行本身不删（历史操作人字段永久保留）；启用只翻标志。
    #[tokio::test]
    async fn disable_cascades_sessions_and_pats_but_keeps_user_row() {
        let (_dir, pool) = migrated_pool().await;
        let repo = UserRepo::new(pool.clone());
        let phc = crate::auth::hash_password("password1").await;
        let admin = repo.create("root", &phc, true).await.expect("admin");
        let user = repo.create("alice", &phc, false).await.expect("目标用户");

        // 预置凭据面：alice 两个 session + 一枚 PAT；root 一个 session
        // （断言级联不误伤他人）。
        sqlx::query("INSERT INTO sessions (id_hash, user_id, created_at, expires_at) VALUES ('h1', ?, 0, 1)")
            .bind(user.id)
            .execute(&pool)
            .await
            .expect("session 1");
        sqlx::query("INSERT INTO sessions (id_hash, user_id, created_at, expires_at) VALUES ('h2', ?, 0, 1)")
            .bind(user.id)
            .execute(&pool)
            .await
            .expect("session 2");
        sqlx::query("INSERT INTO sessions (id_hash, user_id, created_at, expires_at) VALUES ('h-root', ?, 0, 1)")
            .bind(admin.id)
            .execute(&pool)
            .await
            .expect("admin session");
        sqlx::query("INSERT INTO personal_access_tokens (user_id, name, token_hash, expires_at, created_at) VALUES (?, 'ci', 't1', NULL, 0)")
            .bind(user.id)
            .execute(&pool)
            .await
            .expect("PAT");

        let updated = repo
            .set_disabled(user.id, true)
            .await
            .expect("禁用")
            .expect("目标应存在");
        assert!(updated.disabled, "返回行应已禁用");

        // 级联：alice 的凭据面清空，root 的不动。
        let alice_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?")
                .bind(user.id)
                .fetch_one(&pool)
                .await
                .expect("数 session");
        let alice_pats: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM personal_access_tokens WHERE user_id = ?")
                .bind(user.id)
                .fetch_one(&pool)
                .await
                .expect("数 PAT");
        let root_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?")
                .bind(admin.id)
                .fetch_one(&pool)
                .await
                .expect("数 admin session");
        assert_eq!(alice_sessions, 0, "禁用应删全部 session");
        assert_eq!(alice_pats, 0, "禁用应删全部 PAT");
        assert_eq!(root_sessions, 1, "他人 session 不受牵连");

        // 用户行仍在（只禁用不物理删除），密码哈希不动。
        let row = repo.get_by_id(user.id).await.expect("读取").expect("行仍在");
        assert!(row.disabled);
        assert_eq!(row.password_hash, user.password_hash, "禁用不改密码");

        // 启用：翻标志，不复活任何凭据（也不产生新凭据）。
        let reenabled = repo
            .set_disabled(user.id, false)
            .await
            .expect("启用")
            .expect("目标应存在");
        assert!(!reenabled.disabled);
        let alice_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?")
                .bind(user.id)
                .fetch_one(&pool)
                .await
                .expect("再数 session");
        assert_eq!(alice_sessions, 0, "启用不复活 session（重新登录换新）");

        // 未知 id：None（幂等不存在）。
        assert!(
            repo.set_disabled(user.id + 1000, true).await.expect("未知 id").is_none(),
            "未知 id 应 None"
        );
    }

    /// 票 B2b-T4 AC：密码覆写后旧哈希不再匹配、新哈希可校验；目标不存在
    /// 返回 false。
    #[tokio::test]
    async fn set_password_replaces_hash_and_reports_missing_target() {
        let (_dir, pool) = migrated_pool().await;
        let repo = UserRepo::new(pool);
        let old_phc = crate::auth::hash_password("old-password-1").await;
        let user = repo.create("alice", &old_phc, false).await.expect("建用户");

        let new_phc = crate::auth::hash_password("new-password-1").await;
        assert!(
            repo.set_password(user.id, &new_phc).await.expect("覆写"),
            "目标存在应命中"
        );
        let row = repo.get_by_id(user.id).await.expect("读取").expect("行在");
        assert_eq!(row.password_hash, new_phc, "哈希应已替换");
        assert!(
            !crate::auth::verify_password("old-password-1", &row.password_hash).await,
            "旧密码不得再匹配"
        );
        assert!(
            crate::auth::verify_password("new-password-1", &row.password_hash).await,
            "新密码应可校验"
        );

        assert!(
            !repo.set_password(user.id + 1000, &new_phc).await.expect("未知 id"),
            "未知 id 应返回 false"
        );
    }
}

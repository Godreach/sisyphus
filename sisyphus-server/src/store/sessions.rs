//! 会话 repo（票 B2b-T1，ADR-0014）。
//!
//! 主键是 session id 的 SHA-256（[`crate::auth::session_id_hash`]），原始 id
//! 只存在于 cookie——DB 泄露拿不到可用凭据。行在库里，Server 重启不掉线；
//! 滑动过期 = 认证路径 [`SessionRepo::touch`] 顺延；登出/禁用删行。
//! 时间参数显式传入（不取系统时钟），过期语义可直测。

use sqlx::SqlitePool;

use super::StoreError;
use super::is_unique_violation;

/// 会话行（认证路径消费的视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// 属主用户 id。
    pub user_id: i64,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 过期时间（Unix 毫秒，滑动顺延）。
    pub expires_at: i64,
}

/// 会话 repo：写入 / 有效读取 / 顺延 / 删除。
#[derive(Debug, Clone)]
pub struct SessionRepo {
    pool: SqlitePool,
}

impl SessionRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 写入会话行；id 哈希撞主键（32 字节随机碰撞，概率上不可达）返回
    /// [`StoreError::Unique`]，调用侧换 id 重试。
    pub async fn insert(
        &self,
        id_hash: &str,
        user_id: i64,
        created_at: i64,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "INSERT INTO sessions (id_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id_hash)
        .bind(user_id)
        .bind(created_at)
        .bind(expires_at)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(StoreError::Unique(
                "session id 哈希撞主键（随机碰撞，概率上不可达）".into(),
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// 取未过期的会话行（`expires_at <= now` 一律 `None`：过期即认证失败，
    /// 与「行存在与否」不可区分）。
    pub async fn get_valid(
        &self,
        id_hash: &str,
        now: i64,
    ) -> Result<Option<SessionRow>, StoreError> {
        let row = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT user_id, created_at, expires_at FROM sessions
             WHERE id_hash = ? AND expires_at > ?",
        )
        .bind(id_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(user_id, created_at, expires_at)| SessionRow {
            user_id,
            created_at,
            expires_at,
        }))
    }

    /// 顺延过期时间（滑动过期的写侧；只在 `get_valid` 命中后调用）。
    pub async fn touch(&self, id_hash: &str, expires_at: i64) -> Result<(), StoreError> {
        sqlx::query("UPDATE sessions SET expires_at = ? WHERE id_hash = ?")
            .bind(expires_at)
            .bind(id_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 删除会话行（登出即刻失效 / 禁用踢线随用户管理批次复用）。
    pub async fn delete(&self, id_hash: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id_hash = ?")
            .bind(id_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录 + 已迁移库 + 预置用户 root。
    async fn fixture() -> (tempfile::TempDir, SqlitePool, UserRow) {
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
        let phc = crate::auth::hash_password("password1").await;
        let user = super::super::users::UserRepo::new(pool.clone())
            .create("root", &phc, true)
            .await
            .expect("预置用户");
        (dir, pool, user)
    }

    /// 夹具返回的用户行（避免测试依赖 users::User 全形态）。
    type UserRow = super::super::users::User;

    #[tokio::test]
    async fn valid_session_reads_expired_does_not() {
        let (_dir, pool, user) = fixture().await;
        let repo = SessionRepo::new(pool);
        let now = 1_000_000_i64;

        repo.insert("hash-a", user.id, now, now + 60_000)
            .await
            .expect("写入");

        // now 时有效。
        let row = repo
            .get_valid("hash-a", now)
            .await
            .expect("读取")
            .expect("未过期应命中");
        assert_eq!(row.user_id, user.id);

        // 过期瞬间（expires_at == now）：None（严格大于才有效）。
        assert!(
            repo.get_valid("hash-a", now + 60_000)
                .await
                .expect("读取")
                .is_none(),
            "过期会话应认证失败"
        );
        // 不存在的 id 哈希：None。
        assert!(repo.get_valid("hash-x", now).await.expect("读取").is_none());
    }

    #[tokio::test]
    async fn touch_slides_expiry_forward() {
        let (_dir, pool, user) = fixture().await;
        let repo = SessionRepo::new(pool);
        let now = 1_000_000_i64;
        repo.insert("hash-a", user.id, now, now + 60_000)
            .await
            .expect("写入");

        // 认证通过即顺延 7 天。
        let slid_to = now + crate::auth::SESSION_TTL_MS;
        repo.touch("hash-a", slid_to).await.expect("顺延");

        // 原过期点已过、顺延后仍有效（滑动语义的落点）。
        let row = repo
            .get_valid("hash-a", now + 120_000)
            .await
            .expect("读取")
            .expect("顺延后应仍有效");
        assert_eq!(row.expires_at, slid_to);
    }

    #[tokio::test]
    async fn delete_makes_session_invalid_immediately() {
        let (_dir, pool, user) = fixture().await;
        let repo = SessionRepo::new(pool);
        let now = 1_000_000_i64;
        repo.insert("hash-a", user.id, now, now + 60_000)
            .await
            .expect("写入");

        repo.delete("hash-a").await.expect("登出删行");
        assert!(
            repo.get_valid("hash-a", now).await.expect("读取").is_none(),
            "登出后原 session 应即刻失效"
        );
    }

    /// 重启不掉线（票 B2b-T1 AC，store 缝）：行持久在库，二启换池仍可读。
    #[tokio::test]
    async fn session_survives_pool_rebootstrap() {
        let (dir, pool, user) = fixture().await;
        let repo = SessionRepo::new(pool.clone());
        let now = 1_000_000_i64;
        repo.insert("hash-a", user.id, now, now + 60_000)
            .await
            .expect("写入");
        pool.close().await;

        // 同一数据目录重新 bootstrap（模拟 Server 重启）。
        let pool2 = super::super::bootstrap(dir.path())
            .await
            .expect("二启 bootstrap");
        let repo2 = SessionRepo::new(pool2);
        assert!(
            repo2
                .get_valid("hash-a", now)
                .await
                .expect("读取")
                .is_some(),
            "重启后 session 应仍有效"
        );
    }

    #[tokio::test]
    async fn duplicate_id_hash_is_unique_error() {
        let (_dir, pool, user) = fixture().await;
        let repo = SessionRepo::new(pool);
        repo.insert("hash-a", user.id, 0, 1).await.expect("首写");
        let err = repo
            .insert("hash-a", user.id, 0, 1)
            .await
            .expect_err("撞主键应拒绝");
        assert!(matches!(err, StoreError::Unique(_)), "应为唯一冲突：{err}");
    }
}

//! 个人访问令牌 repo（票 B2b-T3，ADR-0014）。
//!
//! token 基座（生成/哈希在 [`crate::auth`]：`sis_` 前缀 + 32 随机字节
//! base64url）的 PAT 落库面：库里只存 token 值的 SHA-256（`token_hash`，
//! 唯一），明文只在创建响应出现一次。吊销 = 删行（下一请求即 401）；
//! `expires_at` 可空（NULL = 永不过期）。Agent token（`sisa_`）复用同一
//! 基座，落表与签发/吊销管理面随 Agent 批次。
//!
//! 时间参数显式传入（不取系统时钟），过期语义可直测（与 sessions 同形：
//! `expires_at <= now` 一律 `None`）。

use sqlx::SqlitePool;

use super::StoreError;
use super::is_unique_violation;

/// PAT 行（认证路径与列表共用视图；`token_hash` 不出 API 面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatRow {
    /// 行 id（吊销端点的路径参数）。
    pub id: i64,
    /// 属主用户 id（权限 = owner 本人）。
    pub user_id: i64,
    /// 令牌名（用户起的管理名）。
    pub name: String,
    /// token 值的 SHA-256 十六进制（明文永不落库）。
    pub token_hash: String,
    /// 过期时间（Unix 毫秒；`None` = 永不过期）。
    pub expires_at: Option<i64>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
}

/// PAT repo：创建 / 列表 / 吊销（删行）/ 按哈希查有效行。
#[derive(Debug, Clone)]
pub struct PatRepo {
    pool: SqlitePool,
}

impl PatRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 写入 PAT 行；token 哈希撞唯一约束（32 字节随机碰撞，概率上不可达）
    /// 返回 [`StoreError::Unique`]，调用侧换 token 重试。
    pub async fn insert(
        &self,
        user_id: i64,
        name: &str,
        token_hash: &str,
        expires_at: Option<i64>,
        created_at: i64,
    ) -> Result<PatRow, StoreError> {
        let result = sqlx::query(
            "INSERT INTO personal_access_tokens (user_id, name, token_hash, expires_at, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(name)
        .bind(token_hash)
        .bind(expires_at)
        .bind(created_at)
        .execute(&self.pool)
        .await;
        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) => {
                return Err(StoreError::Unique(
                    "token 哈希撞唯一约束（随机碰撞，概率上不可达）".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(PatRow {
            id: result.last_insert_rowid(),
            user_id,
            name: name.to_string(),
            token_hash: token_hash.to_string(),
            expires_at,
            created_at,
        })
    }

    /// 列出用户全部 PAT（按创建时间升序；值形态由 API 层裁剪，只有
    /// 名/时间/过期）。
    pub async fn list_by_user(&self, user_id: i64) -> Result<Vec<PatRow>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, i64, String, String, Option<i64>, i64)>(
            "SELECT id, user_id, name, token_hash, expires_at, created_at
             FROM personal_access_tokens WHERE user_id = ? ORDER BY created_at, id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PatRow::from_row).collect())
    }

    /// 吊销（删行，下一请求即 401）。以 id + 属主双条件命中才删——他人
    /// 的 PAT id 不可吊销（返回 `false`，调用侧 404，不暴露存在性）。
    pub async fn delete(&self, user_id: i64, id: i64) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM personal_access_tokens WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 认证路径：按哈希取有效行（过期与吊销同表为 `None`——与「行存在
    /// 与否」不可区分，一律 401）。
    pub async fn find_valid_by_hash(
        &self,
        token_hash: &str,
        now: i64,
    ) -> Result<Option<PatRow>, StoreError> {
        let row = sqlx::query_as::<_, (i64, i64, String, String, Option<i64>, i64)>(
            "SELECT id, user_id, name, token_hash, expires_at, created_at
             FROM personal_access_tokens
             WHERE token_hash = ? AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(PatRow::from_row))
    }
}

impl PatRow {
    /// 手工行映射。
    fn from_row(row: (i64, i64, String, String, Option<i64>, i64)) -> Self {
        Self {
            id: row.0,
            user_id: row.1,
            name: row.2,
            token_hash: row.3,
            expires_at: row.4,
            created_at: row.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{TokenFamily, generate_token, token_hash};

    /// 独立临时目录 + 已迁移库 + 预置用户 root（沿用 sessions 缝形态）。
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
        let phc = crate::auth::hash_password("password1").await;
        let user = super::super::users::UserRepo::new(pool.clone())
            .create("root", &phc, true)
            .await
            .expect("预置用户");
        (dir, pool, user.id)
    }

    /// 生成一个 PAT 的（明文值， 哈希）对。
    fn mint() -> (String, String) {
        let token = generate_token(TokenFamily::Pat);
        let hash = token_hash(&token);
        (token, hash)
    }

    #[tokio::test]
    async fn insert_list_round_trip_and_db_holds_only_hash() {
        let (_dir, pool, user_id) = fixture().await;
        let repo = PatRepo::new(pool.clone());
        let now = 1_000_000_i64;

        let (token, hash) = mint();
        let created = repo
            .insert(user_id, "ci-deploy", &hash, None, now)
            .await
            .expect("写入");
        assert!(created.id > 0);
        assert_eq!(created.token_hash, hash);

        // store 缝直查临时库（票 B2b-T3 AC）：库里只有 SHA-256，明文不落库。
        let stored: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM personal_access_tokens WHERE id = ?")
                .bind(created.id)
                .fetch_optional(&pool)
                .await
                .expect("直查 token_hash");
        assert_eq!(stored.as_deref(), Some(hash.as_str()));
        assert_ne!(stored.as_deref(), Some(token.as_str()));
        let all: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM personal_access_tokens")
            .fetch_all(&pool)
            .await
            .expect("全表直查");
        assert!(
            !all.iter().any(|v| v.contains(&token)),
            "明文 token 值不得出现在任何列"
        );

        // 列表按创建时间序、只含本人的行。
        let (_, hash2) = mint();
        repo.insert(user_id, "nightly", &hash2, Some(now + 86_400_000), now + 1)
            .await
            .expect("第二枚");
        let list = repo.list_by_user(user_id).await.expect("列表");
        assert_eq!(
            list.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["ci-deploy", "nightly"],
            "按创建时间升序"
        );
        assert!(list.iter().all(|r| r.user_id == user_id));
    }

    #[tokio::test]
    async fn duplicate_token_hash_is_unique_error() {
        let (_dir, pool, user_id) = fixture().await;
        let repo = PatRepo::new(pool);
        let (_, hash) = mint();
        repo.insert(user_id, "a", &hash, None, 0)
            .await
            .expect("首写");

        let err = repo
            .insert(user_id, "b", &hash, None, 0)
            .await
            .expect_err("撞唯一约束应拒绝");
        assert!(matches!(err, StoreError::Unique(_)), "应为唯一冲突：{err}");
    }

    #[tokio::test]
    async fn find_valid_respects_expiry_and_absence() {
        let (_dir, pool, user_id) = fixture().await;
        let repo = PatRepo::new(pool);
        let now = 1_000_000_i64;

        // 永不过期：任意时刻有效。
        let (_, forever) = mint();
        repo.insert(user_id, "forever", &forever, None, now)
            .await
            .expect("写入");
        // 带过期：now + 60s。
        let (_, bounded) = mint();
        repo.insert(user_id, "bounded", &bounded, Some(now + 60_000), now)
            .await
            .expect("写入");

        assert!(
            repo.find_valid_by_hash(&forever, now)
                .await
                .expect("查")
                .is_some(),
            "无过期 PAT 应有效"
        );
        let row = repo
            .find_valid_by_hash(&bounded, now)
            .await
            .expect("查")
            .expect("未过期应命中");
        assert_eq!(row.user_id, user_id);

        // 过期瞬间（expires_at == now）：None；未知哈希：None——两者与
        // 吊销在认证面同形。
        assert!(
            repo.find_valid_by_hash(&bounded, now + 60_000)
                .await
                .expect("查")
                .is_none(),
            "过期 PAT 应认证失败"
        );
        let (_, unknown) = mint();
        assert!(
            repo.find_valid_by_hash(&unknown, now)
                .await
                .expect("查")
                .is_none(),
            "未知哈希应 None"
        );
    }

    #[tokio::test]
    async fn delete_revokes_immediately_and_is_owner_scoped() {
        let (_dir, pool, user_id) = fixture().await;
        let repo = PatRepo::new(pool);
        let now = 1_000_000_i64;

        let (_, hash) = mint();
        let row = repo
            .insert(user_id, "doomed", &hash, None, now)
            .await
            .expect("写入");
        assert!(
            repo.find_valid_by_hash(&hash, now)
                .await
                .expect("查")
                .is_some(),
            "吊销前应有效"
        );

        // 他人（user_id + 1）不可吊销：命中 0 行，原行仍在。
        assert!(
            !repo.delete(user_id + 1, row.id).await.expect("他人吊销"),
            "属主外的 delete 不应命中"
        );
        assert!(
            repo.find_valid_by_hash(&hash, now)
                .await
                .expect("查")
                .is_some(),
            "他人吊销后原 token 应仍有效"
        );

        // 属主吊销：删行，下一查即 None（立即失效）；再删同 id 返回 false。
        assert!(repo.delete(user_id, row.id).await.expect("属主吊销"));
        assert!(
            repo.find_valid_by_hash(&hash, now)
                .await
                .expect("查")
                .is_none(),
            "吊销后应立即失效"
        );
        assert!(!repo.delete(user_id, row.id).await.expect("重复吊销"));
    }
}

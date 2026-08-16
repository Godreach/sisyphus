//! 项目机密 repo（票 B2b-T6，ADR-0015）：加密密文落库面。
//!
//! 值只写不读：repo 只提供 建/覆写（upsert）、列名、删——没有任何读值
//! 路径（REST 面亦然，值永无读端点）。ciphertext 形态（版本字节 + nonce +
//! 密文）由加密域逻辑（[`crate::secrets`]）产出，repo 只当不透明字节落库
//! 读回——(project_id, name) 唯一以覆写语义呈现（ON CONFLICT DO UPDATE：
//! 覆写保留 created_at、更新 updated_by/updated_at）。updated_by 为操作人
//! 实名（与 pipeline operator 同纪律，票 B2b-T5）。

use sqlx::SqlitePool;

use super::StoreError;

/// 机密行（repo 内部形态；`ciphertext` 永不出 API 面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRow {
    /// 行 id。
    pub id: i64,
    /// 属主项目 id。
    pub project_id: i64,
    /// 机密名（env 键字符集，API 层校验）。
    pub name: String,
    /// 「版本字节 + nonce + 密文」形态的加密值（不透明字节）。
    pub ciphertext: Vec<u8>,
    /// 最后写入/覆写的操作人（用户名）。
    pub updated_by: String,
    /// 首写时间（覆写保留；Unix 毫秒）。
    pub created_at: i64,
    /// 最后写入时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 项目机密 repo：建/覆写 / 列名 / 删。
#[derive(Debug, Clone)]
pub struct SecretRepo {
    pool: SqlitePool,
}

impl SecretRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 建或覆写机密：`(project_id, name)` 命中即覆写（密文与操作人更新、
    /// 创建时间保留），未命中即新建。时间与操作人由调用侧传入（与 API 层
    /// 的 now 同一取值）。
    pub async fn upsert(
        &self,
        project_id: i64,
        name: &str,
        ciphertext: &[u8],
        updated_by: &str,
        now: i64,
    ) -> Result<SecretRow, StoreError> {
        let row = sqlx::query_as::<_, (i64, i64, String, Vec<u8>, String, i64, i64)>(
            "INSERT INTO secrets (project_id, name, ciphertext, updated_by, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(project_id, name) DO UPDATE SET
                 ciphertext = excluded.ciphertext,
                 updated_by = excluded.updated_by,
                 updated_at = excluded.updated_at
             RETURNING id, project_id, name, ciphertext, updated_by, created_at, updated_at",
        )
        .bind(project_id)
        .bind(name)
        .bind(ciphertext)
        .bind(updated_by)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(SecretRow {
            id: row.0,
            project_id: row.1,
            name: row.2,
            ciphertext: row.3,
            updated_by: row.4,
            created_at: row.5,
            updated_at: row.6,
        })
    }

    /// 项目机密名清单（按名排序输出稳定；只含名，值永不出库面）。
    pub async fn list_names(&self, project_id: i64) -> Result<Vec<String>, StoreError> {
        let names = sqlx::query_scalar("SELECT name FROM secrets WHERE project_id = ? ORDER BY name")
            .bind(project_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(names)
    }

    /// 机密名是否存在（票 B2b-T7：建/覆写前区分审计事件类型——同名已存
    /// 即覆写）。只查存在性，不读值（值只写不读纪律不破）。
    pub async fn exists(&self, project_id: i64, name: &str) -> Result<bool, StoreError> {
        let hit: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM secrets WHERE project_id = ? AND name = ? LIMIT 1",
        )
        .bind(project_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(hit.is_some())
    }

    /// 删除机密（名消失，AC：DELETE 后名不在清单）。以项目 + 名双条件
    /// 命中才删；不存在返回 `false`（调用侧 404，不暴露存在性）。
    pub async fn delete(&self, project_id: i64, name: &str) -> Result<bool, StoreError> {
        let result =
            sqlx::query("DELETE FROM secrets WHERE project_id = ? AND name = ?")
                .bind(project_id)
                .bind(name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::projects::{NewProject, ProjectRepo, ScmType};
    use super::*;
    use crate::secrets::{MasterKey, decrypt, encrypt};

    /// 独立临时目录 + 已迁移库 + 预置项目 demo（沿用 members 缝形态）。
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
                default_branch: None,
            })
            .await
            .expect("建项目");
        (dir, pool, project.id)
    }

    /// 测试密钥（与库形态断言共用）。
    fn test_key() -> MasterKey {
        MasterKey::generate()
    }

    #[tokio::test]
    async fn upsert_and_round_trip_through_db_holds_ciphertext_form() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = SecretRepo::new(pool.clone());
        let key = test_key();
        let plaintext = b"deploy-key-value";
        let now = 1_000_000_i64;

        let blob = encrypt(&key, plaintext).expect("加密");
        let row = repo
            .upsert(project_id, "DEPLOY_KEY", &blob, "alice", now)
            .await
            .expect("建机密");
        assert!(row.id > 0);
        assert_eq!(row.updated_by, "alice");
        assert_eq!(row.created_at, now);
        assert_eq!(row.updated_at, now);

        // store 缝直查临时库（票 B2b-T6 AC）：密文为「版本字节 + nonce +
        // 密文」形态、与明文不等、可解密还原（round-trip 经库）。
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT ciphertext FROM secrets WHERE project_id = ? AND name = ?")
                .bind(project_id)
                .bind("DEPLOY_KEY")
                .fetch_one(&pool)
                .await
                .expect("直查密文");
        assert_eq!(stored, blob, "落库形态即加密域产出");
        assert_eq!(stored[0], crate::secrets::CIPHERTEXT_VERSION, "首字节为版本字节");
        assert_ne!(&stored[1 + crate::secrets::NONCE_LEN..], &plaintext[..], "密文段与明文不等");
        assert_eq!(
            decrypt(&key, &stored).expect("解密"),
            plaintext,
            "经库 round-trip 还原明文"
        );
        // 库形态与明文等长——即便结构上撞巧合，密文也不该等于明文。
        assert_ne!(stored, plaintext, "整体与明文不等");
    }

    #[tokio::test]
    async fn upsert_overwrites_preserving_created_at_and_listing_names_only() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = SecretRepo::new(pool.clone());
        let key = test_key();
        let first = encrypt(&key, b"first").expect("加密");
        let second = encrypt(&key, b"second-value").expect("加密");

        repo.upsert(project_id, "TOKEN", &first, "alice", 1_000)
            .await
            .expect("首写");
        let row = repo
            .upsert(project_id, "TOKEN", &second, "bob", 2_000)
            .await
            .expect("覆写");
        assert_eq!(row.created_at, 1_000, "覆写保留首写时间");
        assert_eq!(row.updated_at, 2_000, "覆写更新时间");
        assert_eq!(row.updated_by, "bob", "覆写更新操作人");
        assert_eq!(row.ciphertext, second, "覆写换密文（新值）");

        // 覆写后库内只有新值（旧密文不残留）。
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT ciphertext FROM secrets WHERE project_id = ? AND name = ?")
                .bind(project_id)
                .bind("TOKEN")
                .fetch_one(&pool)
                .await
                .expect("直查");
        assert_eq!(stored, second);

        // 多机密：列名清单只含名、按名排序、只属于本项目。
        repo.upsert(project_id, "SMTP_PASS", &encrypt(&key, b"smtp").expect("加密"), "alice", 3_000)
            .await
            .expect("第二条");
        let names = repo.list_names(project_id).await.expect("清单");
        assert_eq!(names, vec!["SMTP_PASS", "TOKEN"], "仅名、按名排序");
    }

    #[tokio::test]
    async fn secrets_are_scoped_to_project() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = SecretRepo::new(pool.clone());
        let key = test_key();
        let projects = ProjectRepo::new(pool.clone());
        let other = projects
            .create(NewProject {
                name: "other".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/other".into(),
                default_branch: None,
            })
            .await
            .expect("第二项目");

        repo.upsert(
            project_id,
            "SHARED_NAME",
            &encrypt(&key, b"a").expect("加密"),
            "alice",
            0,
        )
        .await
        .expect("demo 建");
        repo.upsert(
            other.id,
            "SHARED_NAME",
            &encrypt(&key, b"b").expect("加密"),
            "alice",
            0,
        )
        .await
        .expect("other 建");

        // 同名机密跨项目独立（(project, name) 唯一是复合键）。
        assert_eq!(repo.list_names(project_id).await.expect("demo 清单"), vec!["SHARED_NAME"]);
        assert_eq!(repo.list_names(other.id).await.expect("other 清单"), vec!["SHARED_NAME"]);
    }

    #[tokio::test]
    async fn delete_removes_name_and_is_project_scoped() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = SecretRepo::new(pool.clone());
        let key = test_key();
        repo.upsert(project_id, "DOOMED", &encrypt(&key, b"x").expect("加密"), "alice", 0)
            .await
            .expect("建");

        // 他人项目删同名：不命中（项目隔离）。
        assert!(!repo.delete(project_id + 999, "DOOMED").await.expect("异项目删"));
        // 属主删：名消失。
        assert!(repo.delete(project_id, "DOOMED").await.expect("属主删"));
        assert!(repo.list_names(project_id).await.expect("清单").is_empty(), "DELETE 后名消失");
        // 再删同项目同名校：false（不暴露存在性）。
        assert!(!repo.delete(project_id, "DOOMED").await.expect("重复删"));
    }
}

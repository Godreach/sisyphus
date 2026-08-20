//! 项目 SCM 凭据 repo（B5-T3，ADR-0015/0016）：加密密文落库面。
//!
//! 与 0005 secrets 同款「值只写不读」纪律：repo 只提供 [`ScmCredentialRepo::set`]
//! （建/覆写/清）与 [`ScmCredentialRepo::get`]（取 username + 密文，探测路径解密
//! 用）——无明文读路径，REST 面亦无读值端点（与机密同面）。`password_ciphertext`
//! 形态由加密域逻辑（[`crate::secrets`]）产出，repo 当不透明字节落库/读回；
//! `username` 非机密（svn `--username` 进 args、git ASKPASS 读 env），明文落库。
//!
//! 一项目一份（`project_id` 主键）：[`Self::set`] 两参数皆空 = 删行（清凭据），
//! 否则整份替换（PUT 语义：username 与 password 一并落到新值，未给者置空）。
//! 探测路径经 [`Self::get`] 取密文后解密即弃，明文不在任何模块留存。

use sqlx::SqlitePool;

use super::StoreError;

/// SCM 凭据行（repo 内部形态；`password_ciphertext` 永不出 API 面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmCredentialRow {
    /// 属主项目 id（主键）。
    pub project_id: i64,
    /// 用户名（非机密，明文；可空——仅密码凭据少见但允许）。
    pub username: Option<String>,
    /// 密码/token 的「版本字节 + nonce + 密文」形态（不透明字节；可空——
    /// 仅用户名凭据）。
    pub password_ciphertext: Option<Vec<u8>>,
    /// 最后写入操作人（用户名）。
    pub updated_by: String,
    /// 最后写入时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 项目 SCM 凭据 repo：set（建/覆写/清）/ get（取 username + 密文）。
#[derive(Debug, Clone)]
pub struct ScmCredentialRepo {
    pool: SqlitePool,
}

impl ScmCredentialRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 建或覆写 SCM 凭据（PUT 整份替换语义）。`username` 与 `ciphertext` 皆空
    /// = 删行（清凭据）；否则 upsert：两列一并落到新值，未给者置 NULL。
    /// `ciphertext` 由调用侧（API 层）经 [`crate::secrets::encrypt`] 产出。
    pub async fn set(
        &self,
        project_id: i64,
        username: Option<&str>,
        ciphertext: Option<&[u8]>,
        updated_by: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        // 双皆空 = 清凭据（删行；不存在亦无妨）。
        if username.is_none() && ciphertext.is_none() {
            sqlx::query("DELETE FROM project_scm_credentials WHERE project_id = ?")
                .bind(project_id)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        // upsert：主键 project_id 冲突即整份替换（username + 密文 + 操作人 + 时间）。
        sqlx::query(
            "INSERT INTO project_scm_credentials
                 (project_id, username, password_ciphertext, updated_by, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(project_id) DO UPDATE SET
                 username = excluded.username,
                 password_ciphertext = excluded.password_ciphertext,
                 updated_by = excluded.updated_by,
                 updated_at = excluded.updated_at",
        )
        .bind(project_id)
        .bind(username)
        .bind(ciphertext)
        .bind(updated_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 取项目 SCM 凭据（探测路径用：username 明文 + 密文，调用侧解密密码）。
    /// 无行返回 `None`（未配置凭据）。值只写不读纪律不破：本读专为探测/下发
    /// 批次（ADR-0015「解密仅用于探测/Agent 下发」），REST 面无读值端点。
    pub async fn get(&self, project_id: i64) -> Result<Option<ScmCredentialRow>, StoreError> {
        let row = sqlx::query_as::<_, (i64, Option<String>, Option<Vec<u8>>, String, i64)>(
            "SELECT project_id, username, password_ciphertext, updated_by, updated_at
             FROM project_scm_credentials WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ScmCredentialRow {
            project_id: r.0,
            username: r.1,
            password_ciphertext: r.2,
            updated_by: r.3,
            updated_at: r.4,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::projects::{NewProject, ProjectRepo, ScmType};
    use super::*;
    use crate::secrets::{MasterKey, decrypt, encrypt};

    /// 独立临时目录 + 已迁移库 + 预置项目 demo（沿用 secrets 缝形态）。
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

    fn test_key() -> MasterKey {
        MasterKey::generate()
    }

    #[tokio::test]
    async fn set_and_get_round_trips_through_db_holds_ciphertext_form() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
        let key = test_key();
        let blob = encrypt(&key, b"hunter2-token").expect("加密");
        repo.set(project_id, Some("alice"), Some(&blob), "admin", 1_000)
            .await
            .expect("set");

        let row = repo.get(project_id).await.expect("get").expect("应存在");
        assert_eq!(row.project_id, project_id);
        assert_eq!(row.username.as_deref(), Some("alice"));
        assert_eq!(row.updated_by, "admin");
        assert_eq!(row.updated_at, 1_000);
        // 密文形态：版本字节 + nonce + 密文，与明文不等、可解密还原。
        let stored = row.password_ciphertext.expect("有密文");
        assert_eq!(stored, blob);
        assert_eq!(stored[0], crate::secrets::CIPHERTEXT_VERSION);
        assert_ne!(&stored[1 + crate::secrets::NONCE_LEN..], b"hunter2-token");
        assert_eq!(decrypt(&key, &stored).expect("解密"), b"hunter2-token");
    }

    #[tokio::test]
    async fn set_overwrites_replacing_both_username_and_password() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
        let key = test_key();
        let first = encrypt(&key, b"first-pw").expect("加密");
        repo.set(project_id, Some("alice"), Some(&first), "admin", 1_000)
            .await
            .expect("首写");
        // 覆写：username 与 password 一并换（PUT 整份替换）。
        let second = encrypt(&key, b"second-pw").expect("加密");
        repo.set(project_id, Some("bob"), Some(&second), "ops", 2_000)
            .await
            .expect("覆写");
        let row = repo.get(project_id).await.expect("get").expect("应存在");
        assert_eq!(row.username.as_deref(), Some("bob"), "username 换");
        assert_eq!(row.updated_by, "ops");
        assert_eq!(row.updated_at, 2_000);
        assert_eq!(
            decrypt(&key, &row.password_ciphertext.unwrap()).expect("解密"),
            b"second-pw"
        );
    }

    #[tokio::test]
    async fn set_both_none_clears_credential() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
        let key = test_key();
        let blob = encrypt(&key, b"pw").expect("加密");
        repo.set(project_id, Some("alice"), Some(&blob), "admin", 0)
            .await
            .expect("建");
        assert!(repo.get(project_id).await.unwrap().is_some());
        // 双皆空 = 清。
        repo.set(project_id, None, None, "admin", 1)
            .await
            .expect("清");
        assert!(repo.get(project_id).await.unwrap().is_none(), "清后无行");
        // 不存在的项目清亦不报错。
        repo.set(project_id + 999, None, None, "admin", 1)
            .await
            .expect("异项目清不报错");
    }

    #[tokio::test]
    async fn get_returns_none_when_unconfigured() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
        assert!(repo.get(project_id).await.unwrap().is_none(), "未配置无行");
    }

    #[tokio::test]
    async fn credentials_are_scoped_to_project() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
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
        let key = test_key();
        repo.set(
            project_id,
            Some("alice"),
            Some(&encrypt(&key, b"a").unwrap()),
            "admin",
            0,
        )
        .await
        .expect("demo");
        repo.set(
            other.id,
            Some("bob"),
            Some(&encrypt(&key, b"b").unwrap()),
            "admin",
            0,
        )
        .await
        .expect("other");
        assert_eq!(
            repo.get(project_id)
                .await
                .unwrap()
                .unwrap()
                .username
                .as_deref(),
            Some("alice")
        );
        assert_eq!(
            repo.get(other.id)
                .await
                .unwrap()
                .unwrap()
                .username
                .as_deref(),
            Some("bob")
        );
    }

    #[tokio::test]
    async fn set_username_only_stores_plaintext_username_null_password() {
        let (_dir, pool, project_id) = fixture().await;
        let repo = ScmCredentialRepo::new(pool.clone());
        // 仅用户名、无密码（username 明文、password_ciphertext NULL）。
        repo.set(project_id, Some("alice"), None, "admin", 0)
            .await
            .expect("仅用户名");
        let row = repo.get(project_id).await.unwrap().unwrap();
        assert_eq!(row.username.as_deref(), Some("alice"));
        assert!(row.password_ciphertext.is_none(), "无密码");
    }
}

//! 全局 SMTP 配置 repo（B5-T5，ADR-0014/0015）：单行配置（发件 SMTP 连接参数
//! + 发件人），供 notify 批次终态发送读用。
//!
//! 与 0013 SCM 凭据同套加密纪律：`password_ciphertext` 形态由加密域逻辑
//! （[`crate::secrets`]）产出，repo 当不透明字节落库/读回；`username` 非机密
//! （SMTP AUTH 用户名），明文落库。单行表（`id` 恒为 1，迁移 `CHECK` 钉死），
//! [`Self::set`] upsert 整份替换，[`Self::get`] 未配置返回 `None`。`tls` 取值域
//! 由 [`SmtpTls`] 单点收敛（schema 不设 CHECK，同 audit_log 纪律）；get 读回时
//! 解析为枚举，损坏值落 [`StoreError::Invalid`]（正常路径不触发——set 只写
//! `as_str()` 合法值）。
//!
//! 读脱敏纪律不在此层：REST GET 面不回 `password_ciphertext`，回 `password_set`
//! 布尔；本 repo 的 [`Self::get`] 仅供 notify 发送路径（解密密码即用）与 GET 端点
//! （转脱敏形态）消费，密文永不出 API 面。

use sqlx::SqlitePool;
use utoipa::ToSchema;
use serde::{Deserialize, Serialize};

use super::StoreError;

/// SMTP 加密模式（ADR-0015：`tls` 取值域单点收敛）。
///
/// `none`=明文（不加密，仅内网/测试）、`starttls`=STARTTLS 升级（通常 587）、
/// `implicit`=隐式 TLS 全程加密（通常 465）。serde `rename_all=lowercase` 与
/// [`Self::as_str`] 一致——落库字符串与 JSON 形态同形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// 明文（不加密）。
    None,
    /// STARTTLS（明文连接上升级 TLS）。
    StartTls,
    /// 隐式 TLS（连接即 TLS）。
    Implicit,
}

impl SmtpTls {
    /// 全部取值（校验与 OpenAPI 枚举共享）。
    pub const ALL: &[Self] = &[Self::None, Self::StartTls, Self::Implicit];

    /// 落库字符串形态（与 serde 名一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StartTls => "starttls",
            Self::Implicit => "implicit",
        }
    }

    /// 解析落库字符串；未知值返回 `None`（调用侧按损坏处理）。
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// 全局 SMTP 配置行（repo 形态；`password_ciphertext` 永不出 API 面——
/// REST GET 转脱敏 `password_set` 布尔，notify 发送路径解密即用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfigRow {
    /// SMTP 主机。
    pub host: String,
    /// SMTP 端口。
    pub port: i64,
    /// SMTP AUTH 用户名（可空——无认证）。
    pub username: Option<String>,
    /// 密码/token 的「版本字节 + nonce + 密文」形态（不透明字节；可空——无密码）。
    pub password_ciphertext: Option<Vec<u8>>,
    /// 加密模式。
    pub tls: SmtpTls,
    /// 发件人地址（邮件 From）。
    pub from_address: String,
    /// 最后写入操作人（用户名）。
    pub updated_by: String,
    /// 最后写入时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 全局 SMTP 配置 repo：get（取连接参数 + 密文）/ set（建或覆写单行 id=1）。
#[derive(Debug, Clone)]
pub struct SmtpConfigRepo {
    pool: SqlitePool,
}

impl SmtpConfigRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 取全局 SMTP 配置（单行 id=1）；未配置返回 `None`。`tls` 解析为 [`SmtpTls`]，
    /// 损坏值落 [`StoreError::Invalid`]（正常路径不触发）。
    pub async fn get(&self) -> Result<Option<SmtpConfigRow>, StoreError> {
        let row = sqlx::query_as::<_, (String, i64, Option<String>, Option<Vec<u8>>, String, String, String, i64)>(
            "SELECT host, port, username, password_ciphertext, tls, from_address, updated_by, updated_at
             FROM global_smtp_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some(r) => {
                let tls = SmtpTls::parse(&r.4).ok_or_else(|| {
                    StoreError::Invalid(format!("smtp tls 值损坏：{}", r.4))
                })?;
                Some(SmtpConfigRow {
                    host: r.0,
                    port: r.1,
                    username: r.2,
                    password_ciphertext: r.3,
                    tls,
                    from_address: r.5,
                    updated_by: r.6,
                    updated_at: r.7,
                })
            }
            None => None,
        })
    }

    /// 建或覆写全局 SMTP 配置（单行 id=1 upsert）。`ciphertext` 由调用侧经
    /// [`crate::secrets::encrypt`] 产出（`None`=无密码/无认证）；`tls` 落
    /// [`SmtpTls::as_str`]。参数过 clippy too_many_arguments 阈（8 列写入，
    /// 同 grpc.rs:518 先例 allow）。
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        host: &str,
        port: i64,
        username: Option<&str>,
        ciphertext: Option<&[u8]>,
        tls: SmtpTls,
        from_address: &str,
        updated_by: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO global_smtp_config
                 (id, host, port, username, password_ciphertext, tls, from_address, updated_by, updated_at)
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 host = excluded.host, port = excluded.port, username = excluded.username,
                 password_ciphertext = excluded.password_ciphertext, tls = excluded.tls,
                 from_address = excluded.from_address, updated_by = excluded.updated_by,
                 updated_at = excluded.updated_at",
        )
        .bind(host)
        .bind(port)
        .bind(username)
        .bind(ciphertext)
        .bind(tls.as_str())
        .bind(from_address)
        .bind(updated_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{MasterKey, decrypt, encrypt};

    /// 独立临时目录 + 已迁移库（全局配置无需项目前置）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = super::super::bootstrap(dir.path()).await.expect("bootstrap");
        (dir, pool)
    }

    fn test_key() -> MasterKey {
        MasterKey::generate()
    }

    #[tokio::test]
    async fn get_returns_none_when_unconfigured() {
        let (_dir, pool) = fixture().await;
        let repo = SmtpConfigRepo::new(pool.clone());
        assert!(repo.get().await.unwrap().is_none(), "未配置无行");
    }

    #[tokio::test]
    async fn set_then_get_round_trips_through_db_holds_ciphertext_form() {
        let (_dir, pool) = fixture().await;
        let repo = SmtpConfigRepo::new(pool.clone());
        let key = test_key();
        let blob = encrypt(&key, b"smtp-password").expect("加密");
        repo.set(
            "smtp.example.com",
            587,
            Some("postmaster"),
            Some(&blob),
            SmtpTls::StartTls,
            "ci@example.com",
            "admin",
            1_000,
        )
        .await
        .expect("set");

        let row = repo.get().await.expect("get").expect("应存在");
        assert_eq!(row.host, "smtp.example.com");
        assert_eq!(row.port, 587);
        assert_eq!(row.username.as_deref(), Some("postmaster"));
        assert_eq!(row.tls, SmtpTls::StartTls);
        assert_eq!(row.from_address, "ci@example.com");
        assert_eq!(row.updated_by, "admin");
        assert_eq!(row.updated_at, 1_000);
        // 密文形态：版本字节 + nonce + 密文，与明文不等、可解密还原。
        let stored = row.password_ciphertext.expect("有密文");
        assert_eq!(stored, blob);
        assert_eq!(stored[0], crate::secrets::CIPHERTEXT_VERSION);
        assert_ne!(&stored[1 + crate::secrets::NONCE_LEN..], b"smtp-password");
        assert_eq!(decrypt(&key, &stored).expect("解密"), b"smtp-password");
    }

    #[tokio::test]
    async fn set_overwrites_replacing_all_fields() {
        let (_dir, pool) = fixture().await;
        let repo = SmtpConfigRepo::new(pool.clone());
        let key = test_key();
        let first = encrypt(&key, b"first-pw").expect("加密");
        repo.set(
            "smtp-a.example.com",
            25,
            Some("alice"),
            Some(&first),
            SmtpTls::None,
            "a@example.com",
            "admin",
            1_000,
        )
        .await
        .expect("首写");
        // 覆写：整份替换（单行 upsert）。
        let second = encrypt(&key, b"second-pw").expect("加密");
        repo.set(
            "smtp-b.example.com",
            465,
            Some("bob"),
            Some(&second),
            SmtpTls::Implicit,
            "b@example.com",
            "ops",
            2_000,
        )
        .await
        .expect("覆写");
        let row = repo.get().await.unwrap().unwrap();
        assert_eq!(row.host, "smtp-b.example.com", "host 换");
        assert_eq!(row.port, 465);
        assert_eq!(row.username.as_deref(), Some("bob"));
        assert_eq!(row.tls, SmtpTls::Implicit);
        assert_eq!(row.from_address, "b@example.com");
        assert_eq!(row.updated_by, "ops");
        assert_eq!(row.updated_at, 2_000);
        assert_eq!(
            decrypt(&key, &row.password_ciphertext.unwrap()).expect("解密"),
            b"second-pw"
        );
    }

    #[tokio::test]
    async fn set_no_password_stores_null_ciphertext() {
        let (_dir, pool) = fixture().await;
        let repo = SmtpConfigRepo::new(pool.clone());
        // 无认证（username 与 password 皆空）。
        repo.set(
            "internal-relay.local",
            25,
            None,
            None,
            SmtpTls::None,
            "ci@example.com",
            "admin",
            0,
        )
        .await
        .expect("无认证 set");
        let row = repo.get().await.unwrap().unwrap();
        assert!(row.username.is_none());
        assert!(row.password_ciphertext.is_none(), "无密码密文");
        assert_eq!(row.tls, SmtpTls::None);
    }

    #[tokio::test]
    async fn set_is_singleton_only_one_row() {
        let (_dir, pool) = fixture().await;
        let repo = SmtpConfigRepo::new(pool.clone());
        repo.set("a.example.com", 25, None, None, SmtpTls::None, "a@x", "u", 0)
            .await
            .unwrap();
        repo.set("b.example.com", 465, None, None, SmtpTls::Implicit, "b@x", "u", 1)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM global_smtp_config")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "单行表：覆写不增行");
        assert_eq!(repo.get().await.unwrap().unwrap().host, "b.example.com");
    }
}

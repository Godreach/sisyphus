//! 认证域逻辑（ADR-0014，票 B2b-T1）：argon2id 密码哈希 + session id 原语。
//!
//! 本模块只承载纯逻辑（可单测、不依赖 axum）；REST 面（端点与认证中间件）
//! 在 [`crate::api::auth`]。后续批次（PAT、登录限流、policy）在同一模块
//! 扩面（ADR-0009 auth 模块职责）。
//!
//! - 密码：argon2id，OWASP 参数 m=19MiB/t=2/p=1（`19_456` KiB）；落库形态
//!   为 PHC 字符串（自带参数与盐，校验侧无需另存参数）。哈希/校验是
//!   CPU 密集操作（~几十毫秒），经 `spawn_blocking` 移出异步执行器。
//! - session id：32 随机字节 base64url 无填充（43 字符，取值域不含 `=`，
//!   可安全出现在 Cookie 值里）；库里只存其 SHA-256 十六进制——DB 泄露
//!   ≠ 会话劫持。

use argon2::Algorithm;
use argon2::Argon2;
use argon2::Params;
use argon2::Version;
use argon2::password_hash::PasswordHash;
use argon2::password_hash::PasswordHasher;
use argon2::password_hash::PasswordVerifier;
use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::rand_core::RngCore;
use base64ct::Base64UrlUnpadded;
use base64ct::Encoding;
use sha2::Digest;
use sha2::Sha256;

/// 会话 cookie 名（固定，ADR-0014）。
pub const SESSION_COOKIE_NAME: &str = "sisyphus_session";
/// 会话存活时长：7 天滑动过期（毫秒；认证通过即顺延）。
pub const SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// 会话 cookie 的 Max-Age（秒，由 [`SESSION_TTL_MS`] 推导避免双处维护：
/// 浏览器关了再开仍在登录态）。
pub const SESSION_MAX_AGE_SECS: u64 = (SESSION_TTL_MS / 1000) as u64;
/// 密码最小长度（无复杂度规则、无强制过期，ADR-0014）。
pub const MIN_PASSWORD_LEN: usize = 8;

/// 固定参数实例（OWASP：m=19MiB/t=2/p=1）。
fn argon2id() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("合法 OWASP 参数");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// 哈希密码（argon2id，随机盐，返回 PHC 字符串）。
///
/// 失败只可能是 OS 随机源故障（系统级异常），按不可恢复处理。
pub async fn hash_password(password: &str) -> String {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || hash_password_blocking(&password))
        .await
        .expect("spawn_blocking joiner 不 panic")
}

/// [`hash_password`] 的同步形态（spawn_blocking 的载荷；调用侧自担阻塞）。
pub fn hash_password_blocking(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    argon2id()
        .hash_password(password.as_bytes(), &salt)
        .expect("固定合法参数下的哈希不应失败")
        .to_string()
}

/// 校验密码与 PHC 哈希是否匹配；哈希串形态非法视为不匹配（不 panic，
/// 库里出现过脏数据时表现为认证失败）。
pub async fn verify_password(password: &str, phc: &str) -> bool {
    let password = password.to_string();
    let phc = phc.to_string();
    tokio::task::spawn_blocking(move || {
        let Ok(parsed) = PasswordHash::new(&phc) else {
            return false;
        };
        argon2id()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .expect("spawn_blocking joiner 不 panic")
}

/// 生成新 session id：32 随机字节 base64url 无填充（43 字符）。
pub fn generate_session_id() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    Base64UrlUnpadded::encode_string(&bytes)
}

/// session id 的落库/查询形态：SHA-256 十六进制（64 字符）。
pub fn session_id_hash(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    hex(&digest)
}

/// 字节转小写十六进制。
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_produces_owasp_phc_and_verifies() {
        let phc = hash_password("correct horse battery").await;
        assert!(
            phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "PHC: {phc}"
        );

        assert!(verify_password("correct horse battery", &phc).await);
        assert!(!verify_password("wrong password", &phc).await);
    }

    #[tokio::test]
    async fn hash_salts_each_call_and_malformed_hash_fails_closed() {
        let a = hash_password("same password").await;
        let b = hash_password("same password").await;
        assert_ne!(a, b, "随机盐：同密码两次哈希不同");

        // 非法哈希串：false 而非 panic（脏数据表现为认证失败）。
        assert!(!verify_password("x", "not-a-phc-string").await);
    }

    #[test]
    fn session_id_is_43_url_safe_chars_and_hash_is_sha256_hex() {
        let id = generate_session_id();
        assert_eq!(id.len(), 43, "32 字节 base64url 无填充 = 43 字符");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "URL 安全字母表（不含 = / +）：{id}"
        );

        // 确定性 + 已知向量：SHA-256("abc")。
        assert_eq!(session_id_hash("abc"), SESSION_ID_HASH_OF_ABC);
        assert_eq!(session_id_hash("abc").len(), 64);
    }

    /// SHA-256("abc") 的十六进制（标准向量，钉住「确为 SHA-256」）。
    const SESSION_ID_HASH_OF_ABC: &str =
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
}

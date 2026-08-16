//! 认证域逻辑（ADR-0014，票 B2b-T1/T2）：argon2id 密码哈希、session id
//! 原语与登录限流器。
//!
//! 本模块只承载纯逻辑（可单测、不依赖 axum）；REST 面（端点与认证中间件）
//! 在 [`crate::api::auth`]。后续批次（PAT、policy）在同一模块扩面
//! （ADR-0009 auth 模块职责）。
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

/// 登录限流的失败阈值：连续 5 次失败进入冷却（ADR-0014）。
pub const LOGIN_FAILURE_THRESHOLD: u32 = 5;
/// 冷却起始时长（第 1 次触发）。
const LOGIN_COOLDOWN_BASE_MS: i64 = 60 * 1000;
/// 冷却时长封顶（连续触发翻倍递增、封顶 15 分钟，ADR-0014）。
const LOGIN_COOLDOWN_MAX_MS: i64 = 15 * 60 * 1000;

/// 登录限流器（票 B2b-T2，ADR-0014）：进程内内存状态，per-IP 与
/// per-username 双键独立计数（键拼装收在本模块，调用侧只给原始 IP /
/// 用户名）。
///
/// - 连续失败达到 [`LOGIN_FAILURE_THRESHOLD`] 进入冷却，时长随连续触发
///   翻倍递增、封顶 15 分钟；冷却结束后放行（再失败即触发更长冷却）。
/// - 成功登录清零（删键：失败计数与递增档位一并清）。
/// - 重启即清：不落库、不持久锁定——避免恶意以失败登录锁死他人账号
///   （受害者换个入口/等重启即恢复，代价是攻击者重获尝试窗口，取舍
///   见 ADR-0014）。
/// - 时间一律由调用侧传入（Unix 毫秒，与 store 层同纪），假时钟可驱动
///   全部行为；锁内无 await，短临界区不等 IO。
#[derive(Debug, Clone, Default)]
pub struct LoginRateLimiter {
    entries: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Entry>>>,
}

/// 单键记账：连续失败数 + 冷却档位 + 冷却截止。
#[derive(Debug)]
struct Entry {
    failures: u32,
    triggers: u32,
    blocked_until_ms: i64,
}

impl Entry {
    fn new() -> Self {
        Self {
            failures: 0,
            triggers: 0,
            blocked_until_ms: 0,
        }
    }

    /// 当前档位的冷却时长：base × 2^(triggers-1)，封顶 15 分钟。
    fn cooldown_ms(&self) -> i64 {
        let shift = self.triggers.saturating_sub(1).min(31);
        LOGIN_COOLDOWN_BASE_MS
            .saturating_mul(1i64 << shift)
            .min(LOGIN_COOLDOWN_MAX_MS)
    }
}

/// per-IP 键（前缀命名空间收在限流器一侧）。
fn ip_key(ip: &str) -> String {
    format!("ip:{ip}")
}

/// per-username 键。
fn user_key(username: &str) -> String {
    format!("user:{username}")
}

impl LoginRateLimiter {
    /// 新建限流器（空状态）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登录尝试前的双键检查：任一键冷却中则返回剩余毫秒（调用侧回 429）。
    pub fn check_login(&self, ip: &str, username: &str, now_ms: i64) -> Option<i64> {
        let entries = self.entries.lock().expect("限流器锁不中毒");
        [ip_key(ip), user_key(username)]
            .into_iter()
            .filter_map(|key| {
                let entry = entries.get(&key)?;
                (entry.blocked_until_ms > now_ms).then(|| entry.blocked_until_ms - now_ms)
            })
            .min()
    }

    /// 记一次登录失败：双键各记一笔；连续失败达到阈值且当前不在冷却则
    /// 进入冷却（档位递增）。冷却期间的请求在 [`Self::check_login`] 已被
    /// 拦，正常到不了这里。
    pub fn record_login_failure(&self, ip: &str, username: &str, now_ms: i64) {
        let mut entries = self.entries.lock().expect("限流器锁不中毒");
        for key in [ip_key(ip), user_key(username)] {
            let entry = entries.entry(key).or_insert_with(Entry::new);
            entry.failures = entry.failures.saturating_add(1);
            if entry.failures >= LOGIN_FAILURE_THRESHOLD && entry.blocked_until_ms <= now_ms {
                entry.triggers += 1;
                entry.blocked_until_ms = now_ms + entry.cooldown_ms();
            }
        }
    }

    /// 记一次登录成功：双键整条清零（失败计数与递增档位一并清）。
    pub fn record_login_success(&self, ip: &str, username: &str) {
        let mut entries = self.entries.lock().expect("限流器锁不中毒");
        entries.remove(&ip_key(ip));
        entries.remove(&user_key(username));
    }
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

    /// 假时钟基点（任意值；限流器只做时间差运算）。
    const T0: i64 = 1_700_000_000_000;

    #[test]
    fn limiter_allows_below_threshold_and_blocks_at_fifth_failure() {
        let limiter = LoginRateLimiter::new();
        for i in 1..LOGIN_FAILURE_THRESHOLD {
            limiter.record_login_failure("1.2.3.4", "alice", T0);
            assert!(
                limiter.check_login("1.2.3.4", "alice", T0).is_none(),
                "第 {i} 次失败不应冷却"
            );
        }

        // 第 5 次失败进入冷却：剩余 ≈ 60s。
        limiter.record_login_failure("1.2.3.4", "alice", T0);
        let remaining = limiter
            .check_login("1.2.3.4", "alice", T0)
            .expect("第 5 次失败应冷却");
        assert!(
            (59_000..=60_000).contains(&remaining),
            "首档冷却 60s：{remaining}"
        );

        // 冷却结束（含边界恰好到期）：放行。
        let blocked_until = T0 + LOGIN_COOLDOWN_BASE_MS;
        assert!(
            limiter
                .check_login("1.2.3.4", "alice", blocked_until)
                .is_none()
        );
    }

    #[test]
    fn limiter_cooldown_escalates_and_caps_at_15_minutes() {
        let limiter = LoginRateLimiter::new();
        let mut now = T0;
        // 60s → 120s → 240s → 480s → 900s（960 封顶）→ 900s。
        let expected = [60, 120, 240, 480, 900, 900];
        for (n, &secs) in expected.iter().enumerate() {
            // 攒满阈值进入下一轮冷却；冷却结束后先放行一次再失败，
            // 驱动「冷却中不计、结束后一败即再触发」的真实序列。
            loop {
                limiter.record_login_failure("1.2.3.4", "attacker", now);
                match limiter.check_login("1.2.3.4", "attacker", now) {
                    Some(remaining) => {
                        assert!(
                            (secs * 1000 - 1_000..=secs * 1000).contains(&remaining),
                            "第 {} 次触发冷却应 ≈{secs}s：{remaining}",
                            n + 1
                        );
                        now += remaining; // 跳到冷却结束
                        break;
                    }
                    None => continue,
                }
            }
        }
    }

    #[test]
    fn limiter_success_resets_everything_and_keys_are_independent() {
        let limiter = LoginRateLimiter::new();
        for _ in 0..LOGIN_FAILURE_THRESHOLD {
            limiter.record_login_failure("1.2.3.4", "alice", T0);
        }
        assert!(
            limiter.check_login("1.2.3.4", "alice", T0).is_some(),
            "同 IP 同用户名应冷却"
        );

        // 双键独立：ip 键或 user 键各挡各的面——同 IP 换用户名、同用户名
        // 换 IP 都被拦（对应暴破用户名字典 / 撞库两个攻击面）。
        assert!(
            limiter.check_login("1.2.3.4", "bob", T0).is_some(),
            "同 IP 换用户名：ip 键应拦"
        );
        assert!(
            limiter.check_login("5.6.7.8", "alice", T0).is_some(),
            "同用户名换 IP：user 键应拦"
        );
        assert!(
            limiter.check_login("5.6.7.8", "bob", T0).is_none(),
            "双键都全新：不拦"
        );

        // 成功清零：冷却解除，且失败计数归零（1 次失败不再触发）。
        limiter.record_login_success("1.2.3.4", "alice");
        assert!(limiter.check_login("1.2.3.4", "alice", T0).is_none());
        limiter.record_login_failure("1.2.3.4", "alice", T0);
        assert!(
            limiter.check_login("1.2.3.4", "alice", T0).is_none(),
            "清零后 1 次失败不应冷却"
        );
    }
}

//! 机密加密域逻辑（ADR-0015，票 B2b-T6）：主密钥文件 + XChaCha20-Poly1305。
//!
//! - **主密钥文件**：首启自动生成 32 字节随机文件（Unix 0600；Windows 无
//!   POSIX 权限位、尽力而为），已有文件不重生成；路径经 config 可改到
//!   独立卷（运维纵深）。无启动口令、无 OS 钥匙串（ADR-0010 单二进制
//!   零依赖：口令解锁破坏「换二进制即重启」的升级承诺）。
//! - **值加密**：XChaCha20-Poly1305（RustCrypto，纯 Rust、无 AES 硬件
//!   依赖），192 位随机 nonce（免 nonce 管理重用风险）；落库形态为
//!   「版本字节 + nonce + 密文」。版本字节为密钥轮换留口子（轮换本身
//!   v1 不做）。
//! - **防护边界**：防「DB 文件/备份单独泄露」；数据目录整体失守（含
//!   密钥文件）无解，同机 root 不防——写入部署文档（README）。
//!
//! 本模块只承载纯逻辑（可单测、不依赖 axum/SQL）；加密只在机密写入时
//! 调用一次，解密路径不存在——值只写不读，解密仅用于测试与未来下发
//! 批次（engine/Agent）。

use std::path::{Path, PathBuf};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::{Aead, KeyInit};
use rand_core::{OsRng, RngCore};

/// 主密钥字节长（32 字节 = 256 位，XChaCha20 密钥长）。
pub const MASTER_KEY_LEN: usize = 32;
/// 密文形态的版本字节（当前值 1；轮换机制 v1 不做，留口子）。
pub const CIPHERTEXT_VERSION: u8 = 1;
/// XChaCha20-Poly1305 的 nonce 长（192 位 = 24 字节，随机生成免管理）。
pub const NONCE_LEN: usize = 24;
/// 密钥文件权限（Unix 0600：仅属主可读写；Windows 无 POSIX 权限位，
/// 文件权限面由所在卷 ACL 语义承担，尽力而为）。
#[cfg(unix)]
pub const KEY_FILE_PERMS: u32 = 0o600;

/// 主密钥（32 字节）。`Copy` 便于随 [`AppState`](crate::api::AppState) 注入；
/// `Debug` 手工实现打掩码——任何日志路径都不该出现密钥字节。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MasterKey([u8; MASTER_KEY_LEN]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(***)")
    }
}

impl MasterKey {
    /// OS 随机源生成新密钥。
    pub fn generate() -> Self {
        let mut bytes = [0u8; MASTER_KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}

/// 机密加密域错误。
#[derive(Debug)]
pub enum SecretsError {
    /// 密钥文件读写失败。
    Io(std::io::Error),
    /// 密钥文件存在但字节长不对（文件损坏或非本系统生成）。
    BadKeyFile(PathBuf),
    /// 加密失败（AEAD 内部错误，概率上不可达）。
    Encrypt,
    /// 解密失败：AEAD 完整性校验不过（错密钥 / 密文被篡改）。
    Decrypt,
    /// 密文形态非法（短于版本字节 + nonce）。
    BadBlob,
    /// 密文版本字节不是本实现认识的版本（库内出现未来版本数据）。
    UnsupportedVersion(u8),
}

impl std::fmt::Display for SecretsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretsError::Io(e) => write!(f, "密钥文件 IO 错误：{e}"),
            SecretsError::BadKeyFile(path) => {
                write!(
                    f,
                    "主密钥文件字节长非法（应 {MASTER_KEY_LEN}）：{}",
                    path.display()
                )
            }
            SecretsError::Encrypt => write!(f, "加密失败"),
            SecretsError::Decrypt => write!(f, "解密失败（错密钥或密文被篡改）"),
            SecretsError::BadBlob => write!(f, "密文形态非法"),
            SecretsError::UnsupportedVersion(v) => write!(f, "不认识的密文版本字节：{v}"),
        }
    }
}

impl std::error::Error for SecretsError {}

impl From<std::io::Error> for SecretsError {
    fn from(e: std::io::Error) -> Self {
        SecretsError::Io(e)
    }
}

/// 加密明文 → 「版本字节 + 192 位随机 nonce + 密文」落库形态。
///
/// nonce 每次调用随机生成（192 位，碰撞概率上不可达），同明文两次加密
/// 产物不同；调用侧（机密写入路径）不校验、不缓存结果——写一次即落库。
pub fn encrypt(key: &MasterKey, plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);

    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| SecretsError::Encrypt)?;

    let mut out = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    out.push(CIPHERTEXT_VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 解密「版本字节 + nonce + 密文」→ 明文。
///
/// 拒绝：形态短于版本字节 + nonce（[`SecretsError::BadBlob`]）、版本字节
/// 不认识（[`SecretsError::UnsupportedVersion`]）、AEAD 完整性校验失败
/// （[`SecretsError::Decrypt`]——错密钥或密文被篡改，二者不可区分，都按
/// 拒绝对待）。v1 无读值端点，本函数只被测试与未来下发批次消费。
pub fn decrypt(key: &MasterKey, blob: &[u8]) -> Result<Vec<u8>, SecretsError> {
    if blob.len() < 1 + NONCE_LEN {
        return Err(SecretsError::BadBlob);
    }
    if blob[0] != CIPHERTEXT_VERSION {
        return Err(SecretsError::UnsupportedVersion(blob[0]));
    }
    let (nonce, ciphertext) = blob[1..].split_at(NONCE_LEN);

    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let nonce =
        XNonce::from(<[u8; NONCE_LEN]>::try_from(nonce).map_err(|_| SecretsError::BadBlob)?);
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| SecretsError::Decrypt)
}

/// 确保主密钥文件就位并返回密钥（首启路径，ADR-0015）：
///
/// - 文件已存在：不重生成，读回并校验字节长（密钥是加密链唯一锚点，
///   启动时静默换钥 = 全部机密不可解，必须失败出声）。
/// - 文件不存在：生成 32 字节随机文件（原子写：同目录临时文件 + rename，
///   崩溃不留半个密钥文件），权限 0600（Unix；Windows 尽力而为）。
pub fn ensure_master_key(path: &Path) -> Result<MasterKey, SecretsError> {
    if path.is_file() {
        return load_master_key(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let key = MasterKey::generate();
    write_key_file(path, &key)?;
    Ok(key)
}

/// 读回既有密钥文件（已存在文件：配置改路径后从新位置读取）。
pub fn load_master_key(path: &Path) -> Result<MasterKey, SecretsError> {
    let bytes = std::fs::read(path)?;
    let arr: [u8; MASTER_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| SecretsError::BadKeyFile(path.to_path_buf()))?;
    Ok(MasterKey(arr))
}

/// 原子写密钥文件：同目录临时文件写入 + 设权限 + rename（同卷 rename
/// 原子，覆盖目标——首启路径下目标不存在，rename 即落地）。
fn write_key_file(path: &Path, key: &MasterKey) -> Result<(), SecretsError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, key.0)?;
    set_file_perms(&tmp)?;
    std::fs::rename(&tmp, path)?;
    set_file_perms(path)?;
    Ok(())
}

/// 设密钥文件权限。Unix：0600（仅属主）；非 Unix（Windows）：无 POSIX
/// 权限位，尽力而为（AC 原文）——留缝，未来收紧。
#[cfg(unix)]
fn set_file_perms(path: &Path) -> Result<(), SecretsError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEY_FILE_PERMS))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_perms(_path: &Path) -> Result<(), SecretsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加密的固定载荷（round-trip 与库形态断言共用）。
    const PLAINTEXT: &[u8] = b"correct horse battery staple";
    /// 密文总长 = 版本字节 1 + nonce 24 + 明文长 + Poly1305 标签 16。
    fn expected_blob_len(plaintext: &[u8]) -> usize {
        1 + NONCE_LEN + plaintext.len() + 16
    }

    #[test]
    fn encrypt_decrypt_round_trips() {
        let key = MasterKey::generate();
        let blob = encrypt(&key, PLAINTEXT).expect("加密");
        assert_eq!(
            blob.len(),
            expected_blob_len(PLAINTEXT),
            "版本字节 + nonce + 密文（含 16 字节 AEAD 标签）"
        );
        assert_eq!(blob[0], CIPHERTEXT_VERSION, "首字节为版本字节");
        assert_eq!(
            decrypt(&key, &blob).expect("解密"),
            PLAINTEXT,
            "round-trip 还原明文"
        );
    }

    #[test]
    fn wrong_key_and_tamper_are_rejected() {
        let key = MasterKey::generate();
        let blob = encrypt(&key, PLAINTEXT).expect("加密");

        // 错密钥：AEAD 完整性校验失败 → Decrypt。
        let wrong = MasterKey::generate();
        assert!(
            matches!(decrypt(&wrong, &blob), Err(SecretsError::Decrypt)),
            "错密钥必须拒绝"
        );

        // 篡改密文任一字节能被 AEAD 检出（标签校验失败）。
        for at in [0usize, 1, 10, blob.len() - 1] {
            let mut tampered = blob.clone();
            tampered[at] ^= 0x01;
            if at == 0 {
                // 版本字节被改：报 UnsupportedVersion（先于 AEAD 校验）。
                assert!(matches!(
                    decrypt(&key, &tampered),
                    Err(SecretsError::UnsupportedVersion(_))
                ));
            } else {
                assert!(
                    matches!(decrypt(&key, &tampered), Err(SecretsError::Decrypt)),
                    "篡改 {at} 号字节应被拒绝"
                );
            }
        }
    }

    #[test]
    fn same_plaintext_encrypts_differently_and_blob_form_is_stable() {
        let key = MasterKey::generate();
        let a = encrypt(&key, PLAINTEXT).expect("首次加密");
        let b = encrypt(&key, PLAINTEXT).expect("二次加密");
        assert_ne!(a, b, "192 位随机 nonce：同明文两次产物不同");

        // 形态稳定：都带版本字节 + nonce，密文段与明文不等。
        for blob in [&a, &b] {
            assert_eq!(blob[0], CIPHERTEXT_VERSION);
            assert_eq!(
                blob.len(),
                expected_blob_len(PLAINTEXT),
                "密文长度稳定（含随机 nonce 与 AEAD 标签）"
            );
            assert_ne!(
                &blob[1 + NONCE_LEN..],
                PLAINTEXT,
                "密文段与明文不等（非明文自加密兜底形态）"
            );
        }
    }

    #[test]
    fn decrypt_rejects_malformed_blobs() {
        let key = MasterKey::generate();
        assert!(
            matches!(decrypt(&key, &[]), Err(SecretsError::BadBlob)),
            "空 blob"
        );
        assert!(
            matches!(decrypt(&key, &[1, 2, 3]), Err(SecretsError::BadBlob)),
            "短于版本字节 + nonce"
        );
        // 版本字节非 1 的 blob（长度合法）：不认识版本字节。
        let mut future = vec![0u8; 1 + NONCE_LEN + 8];
        future[0] = 99;
        assert!(
            matches!(
                decrypt(&key, &future),
                Err(SecretsError::UnsupportedVersion(99))
            ),
            "不认识版本字节"
        );
    }

    #[test]
    fn key_generates_unique_values() {
        assert_ne!(
            MasterKey::generate(),
            MasterKey::generate(),
            "OS 随机源两次生成应不同"
        );
        assert_eq!(MasterKey::generate().0.len(), MASTER_KEY_LEN);
    }

    #[test]
    fn ensure_generates_then_reuses_existing_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("master.key");

        let first = ensure_master_key(&path).expect("首启生成");
        let written = std::fs::read(&path).expect("文件已落盘");
        assert_eq!(written.len(), MASTER_KEY_LEN, "32 字节随机文件");

        // 二启：已有文件不重生成、读回同一密钥（密钥变化 = 全部机密不可解）。
        let second = ensure_master_key(&path).expect("二启读回");
        assert_eq!(first, second, "已有文件不重生成");
    }

    #[cfg(unix)]
    #[test]
    fn key_file_permissions_are_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("master.key");
        ensure_master_key(&path).expect("生成");
        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "密钥文件仅属主可读写：{mode:o}");
    }

    #[test]
    fn load_rejects_wrong_length_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("master.key");
        std::fs::write(&path, [0u8; 16]).expect("写入 16 字节");
        assert!(
            matches!(
                load_master_key(&path),
                Err(SecretsError::BadKeyFile(p)) if p == path
            ),
            "字节长非法应报 BadKeyFile"
        );

        // 不存在文件：IO 错误。
        let missing = dir.path().join("nope.key");
        assert!(matches!(
            load_master_key(&missing),
            Err(SecretsError::Io(_))
        ));
    }

    #[test]
    fn ensure_creates_parent_directories_for_remote_volume_path() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("secure").join("nested").join("master.key");
        let key = ensure_master_key(&path).expect("生成");
        assert!(path.is_file(), "父目录自动创建、文件落地");
        assert_eq!(key.0.len(), MASTER_KEY_LEN);
    }
}

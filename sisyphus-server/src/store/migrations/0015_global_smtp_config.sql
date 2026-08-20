-- 0015 全局 SMTP 配置表（B5-T5，ADR-0014/0015）
-- 全局资源（ADR-0014：全局 admin 专属）：发件 SMTP 连接参数 + 发件人，供 notify 批次
-- 终态发送读用。单行表（id 恒为 1，CHECK 钉死——全局配置天然单例，不按 owner 多行）。
-- 密码走 ADR-0015 同套加密域（XChaCha20-Poly1305，密文形态 = 版本字节 + 192 位 nonce
-- + 密文，复用 0005 机密 / 0013 SCM 凭据同套逻辑），不立第二机制；可空（SMTP 无认证）。
-- `username` 非机密（SMTP AUTH 用户名，非凭据本体），明文落库。`tls` 取值域由 Rust 枚举
-- （store::smtp_config::SmtpTls）单点收敛，schema 不设 CHECK——同 audit_log 纪律，
-- 新枚举值随批次演进，迁移不追。REST 读侧脱敏（不回密码值，回 password_set 布尔）、
-- 写侧全局 admin 档 + 变更入审计（ADR-0015 全局配置变更是审计事件）。
CREATE TABLE global_smtp_config (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    host             TEXT NOT NULL,
    port             INTEGER NOT NULL,
    username         TEXT,
    password_ciphertext BLOB,
    tls              TEXT NOT NULL,
    from_address     TEXT NOT NULL,
    updated_by       TEXT NOT NULL,
    updated_at       INTEGER NOT NULL
);

-- 0002 用户与会话表（B2b-T1，ADR-0014）
-- users：密码只存 argon2id PHC 字符串（$argon2id$v=19$m=19456,t=2,p=1$...），
-- 明文永不上库。只禁用不物理删除（disabled 列），操作人历史字段永不悬空。
CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0 CHECK (is_admin IN (0, 1)),
    disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

-- sessions：主键为 session id 的 SHA-256（十六进制文本）——库泄露拿不到
-- 原始 id，会话不可劫持（与 PAT 同纪律）。行在库里，故 Server 重启不掉线；
-- 滑动过期由认证路径 UPDATE expires_at 顺延，登出/禁用删行。
CREATE TABLE sessions (
    id_hash    TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

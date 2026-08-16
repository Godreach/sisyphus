-- 0003 个人访问令牌表（B2b-T3，ADR-0014）
-- token 基座（族前缀 + 32 随机字节 base64url）承载的两族 token 之一：
-- PAT（sis_ 前缀）。库里只存 token 值的 SHA-256（十六进制文本）——DB 泄露
-- 拿不到可用凭据（与 sessions.id_hash 同纪律）；吊销 = 删行（即刻失效）；
-- expires_at 可空（NULL = 永不过期）。Agent token（sisa_）随 Agent 批次
-- 落 agents 表，复用同一基座不在本表。
CREATE TABLE personal_access_tokens (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    name       TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_personal_access_tokens_user ON personal_access_tokens(user_id);

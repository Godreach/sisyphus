-- 0005 项目机密表（B2b-T6，ADR-0015）
-- 机密值加密落库：ciphertext 为「版本字节 + 192 位随机 nonce +
-- XChaCha20-Poly1305 密文」（BLOB，形态由加密域逻辑产出，本表只当不透明
-- 字节）。库/备份单独泄露读不出明文——防护边界写进部署文档（README）。
-- (project_id, name) 唯一：建与覆写同走 ON CONFLICT DO UPDATE，覆写保留
-- created_at、updated_by 与 updated_at 更新（覆写语义，不另立 upsert 表）。
-- 值只写不读：repo 层无任何读值方法，v1 REST 面永无读值端点。
CREATE TABLE secrets (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects(id),
    name        TEXT NOT NULL,
    ciphertext  BLOB NOT NULL,
    updated_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

-- 项目维度索引：列名清单（按项目）与删除路径都按 project_id 查。
CREATE INDEX idx_secrets_project ON secrets(project_id);

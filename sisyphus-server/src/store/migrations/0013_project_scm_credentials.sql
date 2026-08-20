-- 0013 项目 SCM 凭据表（B5-T3，ADR-0015/0016）
-- 项目级 SCM 凭据（用户名 + 密码/token）：checkout 自动使用、poll/测试连接
-- 探测解密后递送。值永不上命令行/URL——密码经 XChaCha20-Poly1305 加密落库
-- （密文形态 = 版本字节 + 192 位 nonce + 密文，复用 0005 机密同套加密域逻辑，
-- ADR-0015）；username 非机密（svn `--username` 进 args、git ASKPASS 读 env），
-- 明文落库。凭据只写不读：repo 无明文读路径，仅探测路径解密后即弃。
-- 副表（而非 projects 加列）：与 0005 secrets 同款项目域副表先例，projects
-- 行/Project 结构/各查询不连带膨胀；凭据按 project_id 单行（一项目一份）。
-- ON DELETE CASCADE：项目删（未来批次）连带清凭据，不留孤儿密文。
CREATE TABLE project_scm_credentials (
    project_id          INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    username            TEXT,
    password_ciphertext BLOB,
    updated_by          TEXT NOT NULL,
    updated_at          INTEGER NOT NULL
);

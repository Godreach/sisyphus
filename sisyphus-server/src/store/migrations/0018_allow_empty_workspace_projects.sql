-- no-transaction
-- 0018：支持不绑定 SCM 的空工作区项目。
-- SQLite 无法直接修改 CHECK 约束，重建 projects 表并保留已有数据。
-- 迁移期间必须关闭外键检查：pipelines、成员等表会引用 projects，且 SQLx
-- 迁移事务内无法切换 PRAGMA foreign_keys。
PRAGMA foreign_keys = OFF;

CREATE TABLE projects_v18 (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    scm_type       TEXT NOT NULL CHECK (scm_type IN ('git', 'svn', 'none')),
    scm_url        TEXT NOT NULL,
    default_branch TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

INSERT INTO projects_v18 (id, name, scm_type, scm_url, default_branch, created_at, updated_at)
SELECT id, name, scm_type, scm_url, default_branch, created_at, updated_at
FROM projects;

DROP TABLE projects;
ALTER TABLE projects_v18 RENAME TO projects;

PRAGMA foreign_keys = ON;

-- 0001 初始 schema（B2a-T2）
-- projects：顶层组织单元（CONTEXT.md「项目」词条）。
-- SCM 凭据等列随 SCM/机密批次以加列迁移补（演进：仅加不破）。
CREATE TABLE projects (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    scm_type       TEXT NOT NULL CHECK (scm_type IN ('git', 'svn')),
    scm_url        TEXT NOT NULL,
    default_branch TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

-- pipelines：Pipeline 定义整份 JSON 落库，schema 不解析内部——
-- 校验语义只在 sisyphus-model 单一事实源（Spec B2a）。
-- definition 存 serde JSON 文本；revision 每次保存 +1（ADR-0006）。
CREATE TABLE pipelines (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name       TEXT NOT NULL,
    definition TEXT NOT NULL,
    revision   INTEGER NOT NULL,
    operator   TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

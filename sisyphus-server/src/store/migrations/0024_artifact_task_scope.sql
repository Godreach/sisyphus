-- 产物名只在同一任务 attempt 内唯一；不同任务可声明同名产物而互不覆盖。
-- 旧表的 (build_id, name) 唯一约束会把同构建不同任务的同名产物覆盖掉，
-- 因此重建表并把约束改为 (build_id, job_id, attempt, name)。历史行的归属为空，
-- SQLite 对 NULL 唯一键分组保持兼容。
CREATE TABLE artifacts_scoped (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id INTEGER NOT NULL REFERENCES builds(id),
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retention_until INTEGER NOT NULL,
    backend TEXT NOT NULL DEFAULT 'local' CHECK (backend IN ('local', 's3')),
    job_id INTEGER REFERENCES jobs(id),
    attempt INTEGER,
    state TEXT NOT NULL DEFAULT 'ready' CHECK (state IN ('ready', 'missing', 'pending')),
    UNIQUE (build_id, job_id, attempt, name)
);

INSERT INTO artifacts_scoped
SELECT id, build_id, name, path, size, sha256, created_at, retention_until,
       backend, job_id, attempt, state
FROM artifacts;

DROP TABLE artifacts;
ALTER TABLE artifacts_scoped RENAME TO artifacts;

CREATE INDEX idx_artifacts_retention ON artifacts(retention_until);
CREATE INDEX idx_artifacts_owner ON artifacts(build_id, job_id, attempt);
CREATE INDEX idx_artifacts_backend_retention ON artifacts(backend, retention_until);

-- 0021 产物 pending 状态（票 #123，ADR-0026）。
-- grant 写入 pending（完成前不可见），complete 升 ready。
-- SQLite 不能改 CHECK，重建表以允许 pending。

CREATE TABLE artifacts_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id        INTEGER NOT NULL REFERENCES builds(id),
    name            TEXT NOT NULL,
    path            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    sha256          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    retention_until INTEGER NOT NULL,
    backend         TEXT NOT NULL DEFAULT 'local'
        CHECK (backend IN ('local', 's3')),
    job_id          INTEGER REFERENCES jobs(id),
    attempt         INTEGER,
    state           TEXT NOT NULL DEFAULT 'ready'
        CHECK (state IN ('ready', 'missing', 'pending')),
    UNIQUE (build_id, name)
);

INSERT INTO artifacts_new (
    id, build_id, name, path, size, sha256, created_at, retention_until,
    backend, job_id, attempt, state
)
SELECT
    id, build_id, name, path, size, sha256, created_at, retention_until,
    backend, job_id, attempt, state
FROM artifacts;

DROP TABLE artifacts;
ALTER TABLE artifacts_new RENAME TO artifacts;

CREATE INDEX idx_artifacts_retention ON artifacts(retention_until);
CREATE INDEX idx_artifacts_owner ON artifacts(build_id, job_id, attempt);
CREATE INDEX idx_artifacts_backend_retention ON artifacts(backend, retention_until);

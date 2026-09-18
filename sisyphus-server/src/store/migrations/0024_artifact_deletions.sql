-- 0024 产物异步删除任务（票 #128，ADR-0026）。
-- 未完成任务本身就是 deleting 标记；失败行保留供后台重试和 UI 展示。
ALTER TABLE projects ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'active'
    CHECK (lifecycle IN ('active', 'deleting', 'deleted'));

CREATE TABLE artifact_deletions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id      INTEGER NOT NULL REFERENCES projects(id),
    build_id        INTEGER REFERENCES builds(id),
    -- 历史定位值，不设外键：完成删除后集合元数据会移除，任务仍留作状态记录。
    set_id          INTEGER,
    scope           TEXT NOT NULL CHECK (scope IN ('set', 'build', 'project')),
    state           TEXT NOT NULL CHECK (state IN ('queued', 'running', 'failed', 'completed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at INTEGER NOT NULL,
    requested_by    TEXT NOT NULL,
    source_snapshot TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    CHECK (
        (scope = 'set' AND build_id IS NOT NULL AND set_id IS NOT NULL) OR
        (scope = 'build' AND build_id IS NOT NULL AND set_id IS NULL) OR
        (scope = 'project' AND build_id IS NULL AND set_id IS NULL)
    )
);

CREATE INDEX idx_artifact_deletions_project
    ON artifact_deletions(project_id, created_at DESC);
CREATE INDEX idx_artifact_deletions_retry
    ON artifact_deletions(state, next_attempt_at);
CREATE UNIQUE INDEX idx_artifact_deletions_active_target
    ON artifact_deletions(scope, project_id, IFNULL(build_id, -1), IFNULL(set_id, -1))
    WHERE state != 'completed';

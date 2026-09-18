-- Multipart 临时会话同样按任务 attempt 隔离，避免同名文件互相复用上传许可。
CREATE TABLE artifact_multipart_uploads_scoped (
    build_id INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    object_key TEXT NOT NULL,
    upload_id TEXT NOT NULL,
    size INTEGER NOT NULL,
    part_size INTEGER NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (build_id, job_id, attempt, name)
);
INSERT INTO artifact_multipart_uploads_scoped
SELECT build_id, name, job_id, attempt, object_key, upload_id, size, part_size,
       completed, expires_at, created_at
FROM artifact_multipart_uploads;
DROP TABLE artifact_multipart_uploads;
ALTER TABLE artifact_multipart_uploads_scoped RENAME TO artifact_multipart_uploads;
CREATE INDEX idx_artifact_multipart_expires ON artifact_multipart_uploads(expires_at);

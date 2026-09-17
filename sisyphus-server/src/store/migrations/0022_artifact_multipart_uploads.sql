-- 0022 大文件 multipart 上传会话（票 #124，ADR-0026）。
-- upload id 只授权临时 key；过期行由后台/请求路径 abort 后删除。

CREATE TABLE artifact_multipart_uploads (
    build_id    INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    attempt     INTEGER NOT NULL,
    object_key  TEXT NOT NULL,
    upload_id   TEXT NOT NULL,
    size        INTEGER NOT NULL,
    part_size   INTEGER NOT NULL,
    completed   INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    expires_at  INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (build_id, name)
);

CREATE INDEX idx_artifact_multipart_expires
    ON artifact_multipart_uploads(expires_at);

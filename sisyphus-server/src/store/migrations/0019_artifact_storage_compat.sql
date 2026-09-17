-- 0019 产物存储后端兼容层（票 #121，ADR-0026）。
--
-- 既有行均来自 Server 本地单文件存储：迁移用默认值原位标记为 local / ready，
-- 不搬文件、不改变 (build_id, name) 唯一性与 30 天 retention_until。
-- job_id / attempt 对历史行保持 NULL，因为旧 schema 没有保存上传任务归属；
-- 新上传由应用层写入完整归属。
ALTER TABLE artifacts
    ADD COLUMN backend TEXT NOT NULL DEFAULT 'local'
        CHECK (backend IN ('local', 's3'));

ALTER TABLE artifacts
    ADD COLUMN job_id INTEGER REFERENCES jobs(id);

ALTER TABLE artifacts
    ADD COLUMN attempt INTEGER;

ALTER TABLE artifacts
    ADD COLUMN state TEXT NOT NULL DEFAULT 'ready'
        CHECK (state IN ('ready', 'missing'));

CREATE INDEX idx_artifacts_owner
    ON artifacts(build_id, job_id, attempt);

CREATE INDEX idx_artifacts_backend_retention
    ON artifacts(backend, retention_until);

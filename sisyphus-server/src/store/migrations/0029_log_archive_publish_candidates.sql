-- 并发完成各用独立最终候选 key；复制之前登记。归档删除后保留少量
-- 中断发布的 key 作为 tombstone，避免迟到的 S3 CopyObject 再造永久孤儿。
CREATE TABLE log_archive_publish_candidates (
    key TEXT PRIMARY KEY,
    job_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_log_archive_publish_candidates_archive
    ON log_archive_publish_candidates(job_id, attempt);

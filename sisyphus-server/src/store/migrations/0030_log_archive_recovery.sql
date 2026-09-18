-- 日志状态独立于执行结果；丢失/到期墓碑防止迟到补传复活正文。
ALTER TABLE log_archives ADD COLUMN execution_finished_at INTEGER;
ALTER TABLE log_archives ADD COLUMN lost_reason TEXT;
ALTER TABLE log_archives ADD COLUMN lost_at INTEGER;
UPDATE log_archives SET execution_finished_at = COALESCE(
    (SELECT finished_at FROM jobs WHERE jobs.id = log_archives.job_id), created_at
);
CREATE TABLE agent_log_buffers (
    agent_id INTEGER PRIMARY KEY REFERENCES agents(id),
    bytes INTEGER NOT NULL,
    capacity_bytes INTEGER NOT NULL,
    pending_archives INTEGER NOT NULL,
    pressured INTEGER NOT NULL,
    last_error TEXT,
    reported_at INTEGER NOT NULL
);

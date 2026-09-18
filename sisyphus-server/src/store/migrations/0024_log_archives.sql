-- 0025 任务终态日志归档（ADR-0027 / #129）。旧 logs 表保持原样可读，
-- 新归档以 attempt 为粒度独立登记；正文路径由本地后端维护。
CREATE TABLE log_archives (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs(id),
    attempt INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'ready', 'lost')),
    path TEXT NOT NULL,
    index_path TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    sha256 TEXT NOT NULL DEFAULT '',
    first_seq INTEGER,
    last_seq INTEGER,
    created_at INTEGER NOT NULL,
    ready_at INTEGER,
    UNIQUE(job_id, attempt)
);
CREATE INDEX idx_log_archives_job ON log_archives(job_id, attempt);

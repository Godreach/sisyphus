-- 0011 构建日志落库（票 #73 / B5-T1，ADR-0013/0004）
-- 一行一个日志 chunk：行按 (build, job, attempt, seq) 定位（seq 按 attempt
-- 计、per-job 单调）。data 为 gzip 压缩的 JSONL 事件流（输出块 + 步骤生命
-- 周期事件按到达交织，server 内部 codec 编码；每块独立压缩、范围读取解压
-- 互不依赖）。start_seq..end_seq 为本 chunk 覆盖的连续 seq 区间；step 为
-- chunk 内步骤事件的步骤序号（纯输出/混合为 -1，查询面冗余列）；stream
-- 为 chunk 内输出块的一致 stream 标记（'stdout'/'stderr'，混合/无输出为 ''）。
-- UNIQUE(job_id, attempt, start_seq)：断线补传按 start_seq 幂等去重
-- （Agent 重放天然「从文件头重发未清空段」，重复 seq 忽略、不重不乱序）。
-- 保留清理（per-build 30 天，B5-T6）走 idx_logs_build。
CREATE TABLE logs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id   INTEGER NOT NULL REFERENCES builds(id),
    job_id     INTEGER NOT NULL REFERENCES jobs(id),
    attempt    INTEGER NOT NULL,
    start_seq  INTEGER NOT NULL,
    end_seq    INTEGER NOT NULL,
    step       INTEGER NOT NULL DEFAULT -1,
    stream     TEXT NOT NULL DEFAULT '',
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (job_id, attempt, start_seq)
);

-- 读面：SSE from=<seq> 回放/续传与整份下载共用（按 start_seq 升序）。
CREATE INDEX idx_logs_read ON logs(build_id, job_id, attempt, start_seq);
-- 保留扫描面（per-build 清理，B5-T6 消费）。
CREATE INDEX idx_logs_build ON logs(build_id);

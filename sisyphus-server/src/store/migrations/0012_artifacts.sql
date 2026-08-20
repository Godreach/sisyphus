-- 0012 产物元数据落库（票 #74 / B5-T2，ADR-0004/0006/0007）
-- 一行一份产物：行按 (build, name) 定位（任务级声明的上传名在构建内唯一，
-- 重跑/重试同名再传覆盖为最新）。字节本体在本地磁盘 data/artifacts/
-- <build_id>/<name>（ArtifactStore 缝），此处只存寻址与校验元数据：
-- path 为正斜杠无盘符相对键（v2 对象存储迁移留缝）、size 字节数、
-- sha256 十六进制小写（边写边算，下载响应头回显）。
-- retention_until 为保留期终点（与日志共享 per-build 30 天默认，B5-T6
-- 每日清理扫描消费；idx_artifacts_retention 即扫描面）。
CREATE TABLE artifacts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id        INTEGER NOT NULL REFERENCES builds(id),
    name            TEXT NOT NULL,
    path            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    sha256          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    retention_until INTEGER NOT NULL,
    UNIQUE (build_id, name)
);

-- 保留扫描面（per-build 清理，B5-T6 消费）。
CREATE INDEX idx_artifacts_retention ON artifacts(retention_until);

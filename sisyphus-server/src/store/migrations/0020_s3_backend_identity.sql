-- 0020 S3 后端身份（票 #122，ADR-0026）。
--
-- 记录最近一次成功启用的 endpoint/region/bucket/prefix。已有 S3 对象引用时
-- 若配置被换成另一套身份，启动拒绝，避免把旧对象误读到新桶。
-- 移除 S3 配置可启动，本行保留，供再次配置时比对。
CREATE TABLE s3_backend_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    endpoint TEXT NOT NULL,
    region TEXT NOT NULL,
    bucket TEXT NOT NULL,
    prefix TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
);

-- 0016 日志保留扫描索引（票 #78 / B5-T6，ADR-0013/0004）
-- 每日低频清理扫描面：按 logs.created_at 判「构建的最后日志落库时刻」，
-- 过期构建（cutoff = now - retention_days）的日志 chunk 整批删除、产物文件
-- 与元数据级联删除（空目录回收）；构建记录（状态/号/时长）永久保留。
-- idx_logs_build 已是 per-build 删除路径的索引；本索引补 created_at 过滤面。
CREATE INDEX idx_logs_created ON logs(created_at);

-- 0008 Agent 心跳与磁盘占用（票 #47，Spec B2c §2 + ADR-0007/0019）
-- 在 agents 表上追加磁盘占用列：卷级 free/total + 缓存占用 + 工作区最近
-- 采样（ADR-0019 随心跳上报的两路便宜数据 + 后台采样值）。JSON 文本列、
-- 可空（从未上报为空，详情端点以缺省呈现）——与 system_labels/custom_labels
-- 同为「repo 层解析、schema 不拆内里」的形态。在线判定不依赖本列：
-- online/last_seen_at 由心跳刷（15s 收、45s 无心跳判离线，ADR-0008）。

ALTER TABLE agents ADD COLUMN disk_usage TEXT;

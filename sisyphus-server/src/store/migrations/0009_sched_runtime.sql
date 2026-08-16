-- 0009 sched 运行时时间语义与等待标注（票 B2c-T4，ADR-0008）
-- jobs 增 unknown_at：运行中任务判离线转 unknown 的时刻（Unix 毫秒）。
-- orphan 宽限（默认 10 分钟，config [scheduler]）从此刻计时，超时未恢复判
-- 失败；重连回归 running 时清空。落库为「重启从库重建宽限计时器」的前提
-- （无内存队列，调度状态全在 SQLite，ADR-0008 调度器形态）。
-- jobs 增 waiting_detail：pending 池无匹配 Agent 时的等待原因（缺失标签 /
-- 等待上线 / 等待槽位），供 UI 警示态（ADR-0019 指标「无匹配 Agent/缺标签」
-- 分类）；匹配下发时清空。
ALTER TABLE jobs ADD COLUMN unknown_at INTEGER;
ALTER TABLE jobs ADD COLUMN waiting_detail TEXT;

-- orphan 宽限扫描的查询面：unknown 状态按 unknown_at 排序（部分索引只覆盖
-- 需要的行）。
CREATE INDEX idx_jobs_unknown_due ON jobs(unknown_at) WHERE status = 'unknown';

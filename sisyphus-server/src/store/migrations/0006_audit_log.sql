-- 0006 审计日志表（B2b-T7，ADR-0015）
-- 安全事件的记账与回放：登录成功/失败、登出；用户建/禁（启用同记）、
-- 管理员代办重置密码；PAT 建/销；项目建；成员角色变更；机密建/覆写/删
-- （detail 只记名 + 操作人 + 时间，永不记值）。
-- 只增不改：repo 层不提供任何 UPDATE/DELETE 方法（v1 不做防篡改哈希链，
-- ADR-0015 裁定：单机 SQLite 无独立可信存储，能改审计表的人也能重算链）。
-- actor 为操作人实名（认证用户名，历史字段永不悬空）；project_name 可空
-- （项目域事件记项目名——项目行可能随未来批次删除，审计保留名不保留引用，
-- 与机密/成员的事件回放同纪律）；detail 为 JSON 文本（如机密名、目标用户、
-- 成员角色清单），机密事件只记名不记值。
CREATE TABLE audit_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,   -- 事件时间（Unix 毫秒）
    actor        TEXT NOT NULL,      -- 操作人（用户名）
    event_type   TEXT NOT NULL,      -- 事件类型（取值域由 store::audit 的
                                     -- AuditEvent 单点收敛，schema 不设 CHECK
                                     -- ——新事件类型随批次演进，迁移不追）
    project_name TEXT,               -- 项目名（可空：非项目域事件）
    detail       TEXT                -- JSON 文本（可空：如机密名/目标用户）
);

-- 查询面索引：审计端点按时间 / 用户 / 项目 / 事件类型过滤 + 分页（ts 倒序）。
CREATE INDEX idx_audit_log_ts ON audit_log(ts);
CREATE INDEX idx_audit_log_actor ON audit_log(actor);
CREATE INDEX idx_audit_log_event ON audit_log(event_type);
CREATE INDEX idx_audit_log_project ON audit_log(project_name);

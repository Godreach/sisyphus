-- 0007 调度数据底座（票 #45，Spec B2c §2，ADR-0006/0008/0016）
-- 四表一次建齐：agents / builds / jobs / triggers 的落库面。engine/sched/
-- trigger 的推进逻辑在 B2c 后续批次消费，本迁移只立「调度状态怎么落库、
-- 怎么读回」的形状；状态取值域由 repo 层单点收敛，schema 不设 CHECK
-- （与 audit 同纪律：新状态随批次演进，迁移不追）。

-- agents：构建机注册面（ADR-0008 能力声明；最小注册面，票 B2c-T1）。
-- 双层标签：system_labels 为系统事实标签（sisyphus/os、sisyphus/arch、
-- sisyphus/container 由注册/心跳上报，不可手编）；custom_labels 为管理员
-- 可编辑标签（UI 维护）。二者都存 JSON 数组（key=value 字符串），匹配时
-- 取并集做 AND 全集语义。token_hash 存 per-Agent token 的 SHA-256（sisa_
-- 族，唯一）；register_code_hash 存一次性注册码哈希（注册码换 token 的
-- 完整流程随 Agent 批次，本批由建条目直接签发 token）。disabled 停用即
-- 踢线：sched 不匹配停用 Agent。online/last_seen_at 由心跳维护（45s 无
-- 心跳判离线，ADR-0008），max_concurrency 为并发槽位数（默认 1）。
CREATE TABLE agents (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    name               TEXT NOT NULL UNIQUE,
    token_hash         TEXT NOT NULL UNIQUE,
    system_labels      TEXT NOT NULL,          -- JSON 数组（key=value 字符串）
    custom_labels      TEXT NOT NULL,          -- JSON 数组（key=value 字符串）
    max_concurrency    INTEGER NOT NULL DEFAULT 1,
    online             INTEGER NOT NULL DEFAULT 0,
    disabled           INTEGER NOT NULL DEFAULT 0,
    last_seen_at       INTEGER,                -- 心跳时间（Unix 毫秒；从未在线为空）
    register_code_hash TEXT NOT NULL,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

-- builds：构建行（ADR-0006 构建生命周期）。
-- number 为 per-pipeline 自增构建号（从 1 起）；UNIQUE(project_id,
-- pipeline_name, number) 是并发单调的约束保证（同 pipelines 并发保存
-- 先例：事务内 MAX+1 + 唯一冲突重试，终态号 1..=N 各占一号、不回退）。
-- pipeline_name 冗余自持：快照内亦有定义名，但寻径/编号都走本列。
-- status 取值 queued/running/succeeded/failed/cancelled/timeout；trigger
-- 为触发源 manual/cron/poll；trigger_detail 为 JSON（触发人、分支/commit/
-- revision、参数覆盖——快照不可失的触发上下文）。attempt：从失败任务
-- 重跑同号 attempt+1；从头重跑占新号 attempt=1。snapshot 为 BuildSnapshot
-- JSON（整份 Pipeline 定义 + revision，ADR-0006）；机密只存任务声明的
-- 机密名列表，值永不落快照（ADR-0015）。
CREATE TABLE builds (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id     INTEGER NOT NULL REFERENCES projects(id),
    pipeline_name  TEXT NOT NULL,
    number         INTEGER NOT NULL,
    status         TEXT NOT NULL,
    trigger        TEXT NOT NULL,
    trigger_detail TEXT NOT NULL,              -- JSON（触发上下文）
    attempt        INTEGER NOT NULL DEFAULT 1,
    snapshot       TEXT NOT NULL,              -- BuildSnapshot JSON
    started_at     INTEGER,                    -- queued→running 时刻
    finished_at    INTEGER,                    -- 终态时刻
    cancelled_at   INTEGER,                    -- cancelled 专列
    updated_at     INTEGER NOT NULL,
    UNIQUE (project_id, pipeline_name, number)
);

-- jobs：任务行（ADR-0006 执行语义 + ADR-0008 调度状态）。
-- status 全集合 queued/running/succeeded/failed/cancelled/skipped/unknown/
-- timeout/aborted：unknown 为离线不判死中间态（Agent 侧继续跑，重连回归
-- running）；宽限超时转 failed；Agent 重启丢任务报 aborted（ADR-0008）。
-- (build_id, stage_index, name, attempt) 唯一：重跑同任务占新行 attempt+1，
-- 已成功任务的行与结果保留（ADR-0006 重跑语义）。spec_json 为组装好的
-- ResolvedJobSpec 快照（审计「当时下发什么」；本批只立列，组装随 engine
-- 批次）；agent_id 可空（未调度）；labels 为已匹配回显（JSON 数组）；
-- timeout_minutes/retry_count/allow_failure 从定义快照冗余（任务级执行
-- 语义，调度只信快照）。
CREATE TABLE jobs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    build_id        INTEGER NOT NULL REFERENCES builds(id),
    stage_index     INTEGER NOT NULL,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempt         INTEGER NOT NULL,
    spec_json       TEXT,                      -- ResolvedJobSpec JSON（可空）
    agent_id        INTEGER REFERENCES agents(id),
    labels          TEXT NOT NULL,             -- JSON 数组（已匹配回显）
    timeout_minutes INTEGER NOT NULL DEFAULT 0,
    retry_count     INTEGER NOT NULL DEFAULT 0,
    allow_failure   INTEGER NOT NULL DEFAULT 0,
    started_at      INTEGER,
    finished_at     INTEGER,
    exit_code       INTEGER,                   -- 可空：终态可留退出码
    detail          TEXT,
    UNIQUE (build_id, stage_index, name, attempt)
);

-- triggers：定时/轮询触发源（ADR-0016；manual 触发不建行）。
-- kind 为 cron/poll（同 pipeline 各一）；spec 为 JSON（cron 表达式或 poll
-- 节奏）；enabled 启停；baseline_commit 为 poll 基线（创建/启用时记录、
-- 不触发，只对之后的新提交触发，commit-id 去重）；last_probe_at /
-- last_probe_error 为探测历史（失败记入、继续按节奏重试、不自动禁用）。
CREATE TABLE triggers (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id       INTEGER NOT NULL REFERENCES projects(id),
    pipeline_name    TEXT NOT NULL,
    kind             TEXT NOT NULL,            -- cron/poll
    spec             TEXT NOT NULL,            -- JSON
    enabled          INTEGER NOT NULL DEFAULT 1,
    baseline_commit  TEXT,
    last_probe_at    INTEGER,
    last_probe_error TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (project_id, pipeline_name, kind)
);

-- 查询面索引：构建列表按号倒序 / per-pipeline FIFO 排队（最老 queued）；
-- 槽位统计（running/unknown 在途任务占槽，ADR-0008）；触发器扫表。
CREATE INDEX idx_builds_pipeline ON builds(project_id, pipeline_name);
CREATE INDEX idx_jobs_build ON jobs(build_id);
CREATE INDEX idx_jobs_agent_active ON jobs(agent_id) WHERE status IN ('running', 'unknown');
CREATE INDEX idx_triggers_project ON triggers(project_id, pipeline_name);

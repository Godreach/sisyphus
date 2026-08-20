-- 0014 Agent 版本/升级状态/排空/升级指令持久化 + 升级包元数据
-- （票 #76 / B5-T4，ADR-0017/0011/0012）。
--
-- 版本与状态进契约（ADR-0017）：agents 加列承载握手上报的 agent 版本、
-- 升级阶段、排空/升级中标记、待补发的升级指令；升级指令持久化使离线
-- Agent 重连后补发（与取消指令同机制，ADR-0008）。列皆可空/有默认，前向
-- 迁移安全（既有行 = 从未握手/无升级）。
--
--   agent_version   TEXT          -- JSON {major,minor,patch}，null = 从未握手
--   upgrade_phase   TEXT          -- draining/downloading/swapping/restarting/fallback
--                                  --   null = 无升级在进行
--   upgrade_error   TEXT          -- 升级失败原因（fallback 时记，否则 null）
--   pending_upgrade TEXT          -- JSON {package_name,sha256,download_url}：
--                                  --   已下发但未收 UpgradeStatus 回执的升级指令
--                                  --   （离线 Agent 重连补发面），null = 无待补发
--
-- 升级包元数据（ADR-0017：已上传包清单——版本/目标三元组/sha256）。包字节
-- 本体存 data/upgrade-packages/<package_name>（UpgradePackageStore 缝），此处
-- 只存寻址与校验元数据。package_name 即 ADR-0010 文件名规范
-- sisyphus-agent-<ver>-<os>-<arch>（上传时解析出 version/target_os/target_arch），
-- 全局唯一（同名再传覆盖为最新）。version 为 JSON {major,minor,patch}，窗口
-- 校验（≥ N-1 且 ≤ Server 版本）在上传端点裁决，窗外拒收（ADR-0017）。

ALTER TABLE agents ADD COLUMN agent_version TEXT;
ALTER TABLE agents ADD COLUMN upgrade_phase TEXT;
ALTER TABLE agents ADD COLUMN upgrade_error TEXT;
ALTER TABLE agents ADD COLUMN pending_upgrade TEXT;

CREATE TABLE upgrade_packages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    package_name TEXT NOT NULL UNIQUE,
    version      TEXT NOT NULL,
    target_os    TEXT NOT NULL,
    target_arch  TEXT NOT NULL,
    size         INTEGER NOT NULL,
    sha256       TEXT NOT NULL,
    created_at   INTEGER NOT NULL
);

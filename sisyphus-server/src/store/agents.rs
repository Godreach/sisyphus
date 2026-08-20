//! Agent 注册面 repo（票 B2c-T1，Spec B2c §2 + 最小注册面，ADR-0008）。
//!
//! 双层标签（ADR-0008 能力声明）：system_labels 为系统事实标签（sisyphus/os、
//! sisyphus/arch、sisyphus/container 由注册/心跳上报，不可手编）；custom_labels
//! 为管理员可编辑标签。都存 JSON 数组（key=value 字符串），匹配取并集做
//! AND 全集语义——Agent 必须拥有 job 要求的全部标签（[`AgentRepo::match_job`]）。
//! 容器任务由调用侧先行追加 `sisyphus/container=docker` 到标签要求，repo
//! 只做纯 AND 匹配（隐式追加语义在 sched/engine 面，本缝不猜）。
//!
//! token_hash 存 per-Agent token 的 SHA-256（sisa_ 族，唯一）；register_code_hash
//! 存一次性注册码哈希（注册码换 token 流程随 Agent 批次，本批建条目即签
//! 发）。disabled 停用即踢线：匹配面不命中停用 Agent。槽位占用计数经
//! [`crate::store::jobs::JobRepo::active_by_agent`]（running/unknown 在途，
//! ADR-0008），[`Self::has_slots`] 供 sched 判定。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{StoreError, is_unique_violation, now_ms};

// 版本比较是 Agent/Server 共享契约（ADR-0007：唯一共享 crate 单点维护），
// 故本 repo 直接复用 `sisyphus-proto` 的 `peer_too_old`/`peer_too_new` 而不
// 另写一套——N-1 下界逻辑漂移才是真陷阱。`disk_usage` 等结构仍由 gRPC 面做
// proto↔store 转换（store 不持 proto 消息类型），版本比较是这一纪律的例外。
use sisyphus_proto::agent::Version as ProtoVersion;
use sisyphus_proto::version as version_window;

/// Agent 版本（JSON 文本列 `agents.agent_version` 的落库形态；与 proto
/// `Version` 同构——比较时经 [`Self::to_proto`] 走 proto 的 N-1 窗口逻辑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentVersion {
    /// 主版本。
    pub major: u32,
    /// 次版本。
    pub minor: u32,
    /// 补丁版本。
    pub patch: u32,
}

impl AgentVersion {
    /// 从 proto `Version` 转（gRPC 握手面上报后落库前的转换）。
    pub fn from_proto(v: &ProtoVersion) -> Self {
        Self {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
        }
    }

    /// 转 proto `Version`（比较走 proto 的窗口判定）。AgentVersion 为 Copy，按值取。
    fn to_proto(self) -> ProtoVersion {
        ProtoVersion {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
        }
    }

    /// 是否过旧（< N-1 下界）：任务面拒派、升级面保留（ADR-0017）。
    pub fn too_old(&self, server: &AgentVersion) -> bool {
        version_window::peer_too_old(&self.to_proto(), &server.to_proto())
    }

    /// 是否过新（> Server）：握手即拒连（ADR-0010），连上的 Agent 不会过新。
    pub fn too_new(&self, server: &AgentVersion) -> bool {
        version_window::peer_too_new(&self.to_proto(), &server.to_proto())
    }

    /// 是否落在 N-1 兼容窗口内（既不过新也不过旧）——可派发。
    pub fn in_window(&self, server: &AgentVersion) -> bool {
        version_window::peer_in_window(&self.to_proto(), &server.to_proto())
    }
}

/// 待补发的升级指令（JSON 文本列 `agents.pending_upgrade`；与 proto
/// `UpgradeCommand` 三字段同构）。已下发但未收 `UpgradeStatus` 回执——离线
/// Agent 重连补发面（ADR-0017，与取消指令同机制）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpgrade {
    /// 发行包文件名（ADR-0010 规范 `sisyphus-agent-<ver>-<os>-<arch>`）。
    pub package_name: String,
    /// 包的 SHA-256（十六进制小写；Agent 下载后校验）。
    pub sha256: String,
    /// 包下载 URL（相对路径，Agent 侧按 `api_url` 解析为绝对）。
    pub download_url: String,
}

impl PendingUpgrade {
    /// 映射为 proto `UpgradeCommand`（下发与补发共用，单点构造避免两处字段
    /// 拷贝漂移）。`download_url` 为相对路径，Agent 侧按 `api_url` 解析。
    pub fn to_upgrade_command(&self) -> sisyphus_proto::agent::UpgradeCommand {
        sisyphus_proto::agent::UpgradeCommand {
            package_name: self.package_name.clone(),
            sha256: self.sha256.clone(),
            download_url: self.download_url.clone(),
        }
    }
}

/// 卷级磁盘占用（ADR-0019：随心跳上报的 statvfs/GetDiskFreeSpaceEx 值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    /// 挂载点/盘符。
    pub mount_point: String,
    /// 总量（字节）。
    pub total_bytes: i64,
    /// 剩余量（字节）。
    pub free_bytes: i64,
}

/// Agent 磁盘占用上报的落库形态（JSON 文本列 `agents.disk_usage`，可空；
/// 解析收在 [`AgentRow::disk_usage`] 返回前，schema 不拆内里）。
///
/// 与 proto `DiskUsage` 同构（卷级 + 缓存占用 + 工作区最近采样），由
/// gRPC 面把 proto 消息转成此形态落库（store 不依赖 proto，转换在调用侧）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDiskUsage {
    /// 卷级剩余/总量（多卷逐项）。
    pub volumes: Vec<VolumeUsage>,
    /// 缓存占用（记账值，ADR-0012 registry）。
    pub cache_bytes: i64,
    /// 工作区占用最近采样（ADR-0019，Agent 后台任务降频采样）。
    pub workspace_bytes: i64,
}

/// Agent 行（`system_labels`/`custom_labels`/`disk_usage` 为 JSON 文本——
/// 匹配取并集由 [`Self::all_labels`] 收敛、磁盘占用经 [`Self::disk_usage`]
/// 解析，schema 不解析内部）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// 行 id（jobs.agent_id 引用）。
    pub id: i64,
    /// 构建机名（唯一）。
    pub name: String,
    /// Agent token 的 SHA-256（明文永不落库）。
    pub token_hash: String,
    /// 系统事实标签 JSON（sisyphus/os、sisyphus/arch、sisyphus/container）。
    pub system_labels: String,
    /// 自定义标签 JSON（管理员可编辑）。
    pub custom_labels: String,
    /// 并发槽位数（默认 1）。
    pub max_concurrency: i32,
    /// 是否在线（心跳 45s 无更新判离线，ADR-0008）。
    pub online: bool,
    /// 是否停用（停用即踢线，不匹配任务）。
    pub disabled: bool,
    /// 最近心跳时间（Unix 毫秒）。
    pub last_seen_at: Option<i64>,
    /// 磁盘占用 JSON（ADR-0019；从未上报为空）。
    pub disk_usage: Option<String>,
    /// 一次性注册码的 SHA-256（注册码换 token 流程随 Agent 批次）。
    pub register_code_hash: String,
    /// 注册码是否已兑（一次性置位：兑码即 1，防重放，票 #57）。
    pub register_code_used: bool,
    /// 注册码有效期截止（Unix 毫秒；ADR-0010：一次性 + 24h 过期。迁移前
    /// 既有行为空 = 不失效的遗留语义）。
    pub register_code_expires_at: Option<i64>,
    /// 握手上报的 Agent 版本 JSON（ADR-0017；null = 从未握手）。
    pub agent_version: Option<String>,
    /// 升级阶段（draining/downloading/swapping/restarting/fallback；null =
    /// 无升级在进行，ADR-0017）。
    pub upgrade_phase: Option<String>,
    /// 升级失败原因（fallback 时记；否则 null）。
    pub upgrade_error: Option<String>,
    /// 待补发升级指令 JSON（[`PendingUpgrade`]；null = 无待补发）。
    pub pending_upgrade: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 升级中阶段集合（排空/下载/换入/重启中——不派新任务；fallback 与 null
/// 视为可派发，ADR-0017）。
const MID_UPGRADE_PHASES: &[&str] = &["draining", "downloading", "swapping", "restarting"];

impl AgentRow {
    /// 全部标签取并集（系统 + 自定义；匹配语义的输入集合）。
    pub fn all_labels(&self) -> Result<Vec<String>, StoreError> {
        let mut labels: Vec<String> =
            serde_json::from_str(&self.system_labels).map_err(StoreError::DefinitionJson)?;
        let custom: Vec<String> =
            serde_json::from_str(&self.custom_labels).map_err(StoreError::DefinitionJson)?;
        labels.extend(custom);
        Ok(labels)
    }

    /// 磁盘占用解析（从未上报为 `None`；脏 JSON 视为库损坏）。
    pub fn disk_usage(&self) -> Result<Option<AgentDiskUsage>, StoreError> {
        self.disk_usage
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::DefinitionJson)
    }

    /// 握手版本解析（从未握手为 `None`；脏 JSON 视为库损坏）。
    pub fn agent_version(&self) -> Result<Option<AgentVersion>, StoreError> {
        self.agent_version
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::DefinitionJson)
    }

    /// 待补发升级指令解析（无待补发为 `None`；脏 JSON 视为库损坏）。
    pub fn pending_upgrade(&self) -> Result<Option<PendingUpgrade>, StoreError> {
        self.pending_upgrade
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::DefinitionJson)
    }

    /// 是否处于升级中（Agent 已上报 draining/downloading/swapping/restarting 阶段）。
    /// 不含 `pending_upgrade`——「已上报」是诚实分界：pending 是「已下发未回执」，
    /// Agent 尚未开始排空（离线时更是从未收到）。UI 徽标据此展示「排空」，
    /// 避免对「指令已排队但 Agent 还没动」的 Agent 误报排空。
    pub fn upgrade_active(&self) -> bool {
        self.upgrade_phase
            .as_deref()
            .is_some_and(|p| MID_UPGRADE_PHASES.contains(&p))
    }

    /// 是否处于升级中（排空/下载/换入/重启中，**或有待补发指令**）：不派新任务
    /// （ADR-0017「在线但不可派发」）。fallback 与无升级视为可派发。
    ///
    /// 比 [`Self::upgrade_active`] 多含 `pending_upgrade`——这是**调度派发门**的
    /// 语义：指令已排队即不应再派新任务（否则新任务下发后即被 Agent 排空拒
    /// 收），把「下发到回执」窗口也关闭。UI 徽标用 [`Self::upgrade_active`]，
    /// 调度派发门用本方法。
    pub fn mid_upgrade(&self) -> bool {
        self.pending_upgrade.is_some() || self.upgrade_active()
    }

    /// 是否版本不兼容（过旧或过新；ADR-0017 任务面拒连）。脏 `agent_version`
    /// JSON 视为库损坏——返回 `Err` 而非静默判兼容（否则损坏数据会绕过 N-1
    /// 拒派门）。从未握手（无版本）视为兼容——无版本的 Agent 不会在线（握手
    /// 即落版本），离线者本就不派。
    pub fn version_incompatible(
        &self,
        server: &AgentVersion,
    ) -> Result<bool, StoreError> {
        match self.agent_version()? {
            Some(v) => Ok(v.too_old(server) || v.too_new(server)),
            None => Ok(false),
        }
    }

    /// 是否可派发（在线 + 未停用 + 非升级中 + 版本兼容；槽位由
    /// [`AgentRepo::has_slots`] 另判）。调度候选过滤的单点谓词。脏
    /// `agent_version` JSON 透传为 `Err`（库损坏不静默放行）。
    pub fn dispatchable(&self, server: &AgentVersion) -> Result<bool, StoreError> {
        Ok(self.online && !self.disabled && !self.mid_upgrade() && !self.version_incompatible(server)?)
    }
}

/// 建条目后立刻签发 token（最小注册面）在调用侧组装：本 repo 只落库行，
/// token 生成与哈希经 [`crate::auth`] 基座（与 PAT 同纪律：明文只在创建
/// 响应出现一次）。disk_usage 不在建条目面：随心跳上报（ADR-0019）。
#[derive(Debug, Clone)]
pub struct NewAgent {
    /// 构建机名（唯一）。
    pub name: String,
    /// Agent token 的 SHA-256（sisa_ 族）。
    pub token_hash: String,
    /// 系统标签 JSON 数组文本（首建时可为空数组，注册/心跳上报）。
    pub system_labels: String,
    /// 自定义标签 JSON 数组文本。
    pub custom_labels: String,
    /// 并发槽位数。
    pub max_concurrency: i32,
    /// 一次性注册码的 SHA-256。
    pub register_code_hash: String,
    /// 注册码有效期截止（Unix 毫秒；建条目即签 24h，ADR-0010）。
    pub register_code_expires_at: i64,
}

/// Agent repo：建条目 / 启停与编辑 / 在线维护 / 标签匹配 / 升级状态。
#[derive(Debug, Clone)]
pub struct AgentRepo {
    pool: SqlitePool,
    /// Server 版本（N-1 窗口判定基准；默认即 `sisyphus_proto::version::VERSION`，
    /// 全程不变——同版本成对发布，ADR-0010）。持有在 repo 以免
    /// [`Self::match_candidates`] 等过滤面反复传参。
    server_version: AgentVersion,
}

impl AgentRepo {
    /// 以连接池构造（Server 版本取当前发行 `sisyphus_proto::version::VERSION`；
    /// 版本是编译期常量、运行期不变，故默认即正确，无需调用侧传参）。
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            server_version: AgentVersion::from_proto(&version_window::VERSION),
        }
    }

    /// Server 版本（调度/派生面的 N-1 窗口基准）。
    pub fn server_version(&self) -> AgentVersion {
        self.server_version
    }

    /// 建 Agent 条目（离线、未禁用）；名称撞唯一约束返回
    /// [`StoreError::Unique`]。
    pub async fn create(&self, input: NewAgent) -> Result<AgentRow, StoreError> {
        let now = now_ms();
        let result = sqlx::query(
            "INSERT INTO agents
                (name, token_hash, system_labels, custom_labels, max_concurrency,
                 online, disabled, register_code_hash, register_code_expires_at,
                 created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?, ?, ?)",
        )
        .bind(&input.name)
        .bind(&input.token_hash)
        .bind(&input.system_labels)
        .bind(&input.custom_labels)
        .bind(input.max_concurrency)
        .bind(&input.register_code_hash)
        .bind(input.register_code_expires_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        let result = match result {
            Ok(result) => result,
            Err(e) if is_unique_violation(&e) => {
                return Err(StoreError::Unique(format!(
                    "Agent 名已存在：{}",
                    input.name
                )));
            }
            Err(e) => return Err(e.into()),
        };
        self.get(result.last_insert_rowid())
            .await
            .map(|row| row.expect("刚插入的行必存在"))
    }

    /// 启停（disabled=1 即踢线：不匹配任务；token 仍在表，吊销语义由调用
    /// 侧按「停用即无效」执行）。返回 false 表示 Agent 不存在。
    pub async fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE agents SET disabled = ?, updated_at = ? WHERE id = ?")
            .bind(disabled)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 编辑槽位与自定义标签（UI 维护面；PATCH 语义由调用侧传目标值）。
    /// 返回 false 表示 Agent 不存在。
    pub async fn update_spec(
        &self,
        id: i64,
        max_concurrency: i32,
        custom_labels: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE agents SET max_concurrency = ?, custom_labels = ?, updated_at = ? WHERE id = ?",
        )
        .bind(max_concurrency)
        .bind(custom_labels)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 上线（注册/心跳首次确认）：置 online、刷 last_seen、更新系统标签
    /// 与建议并发（注册/心跳上报的 os/arch/container 事实）。系统标签为
    /// 事实面：整组替换（调用侧负责组装当前探测结果）。
    pub async fn mark_online(
        &self,
        id: i64,
        system_labels: &str,
        suggested_concurrency: Option<i32>,
        seen_at: i64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE agents
             SET online = 1, system_labels = ?, last_seen_at = ?,
                 max_concurrency = COALESCE(?, max_concurrency), updated_at = ?
             WHERE id = ?",
        )
        .bind(system_labels)
        .bind(seen_at)
        .bind(suggested_concurrency)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 心跳上报：刷在线与 last_seen、整组替换系统标签（连接面事实，随每
    /// 次心跳重写保持最新）、落磁盘占用（首次上报即写入，之后随心跳覆盖；
    /// 上报 `None` 不清旧值）。停用即踢线：`disabled` Agent 的心跳不生效
    /// （返回 false，通道面据此断开——与认证面同纪律，不等待下次连接受拒）。
    pub async fn heartbeat(
        &self,
        id: i64,
        system_labels: &str,
        disk_usage: Option<&str>,
        seen_at: i64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE agents
             SET online = 1, system_labels = ?, last_seen_at = ?,
                 disk_usage = COALESCE(?, disk_usage), updated_at = ?
             WHERE id = ? AND disabled = 0",
        )
        .bind(system_labels)
        .bind(seen_at)
        .bind(disk_usage)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 离线（心跳超时判定）：置 online=0、刷 last_seen。运行中任务由
    /// sched 转 unknown，Agent 侧继续跑（ADR-0008 离线不判死）。
    pub async fn mark_offline(&self, id: i64, at: i64) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE agents SET online = 0, last_seen_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(at)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 认证面：按 token 哈希取未停用 Agent（停用即踢线：认证失败）。
    /// 不存在或已停用返回 `None`（与「行不存在」不可区分，一律拒连）。
    pub async fn find_active_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents WHERE token_hash = ? AND disabled = 0",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::from_tuple).transpose()
    }

    /// 注册码换 token 流程的查行面（票 #57）：按注册码哈希取 Agent——含
    /// 一次性/有效期/停用裁决所需的全部字段，由调用侧（REST 面）裁决并
    /// 调 [`Self::redeem_register_code`] 原子置已用。不存在返回 `None`
    /// （无效注册码）。
    pub async fn find_by_register_code(
        &self,
        code_hash: &str,
    ) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents WHERE register_code_hash = ?",
        )
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::from_tuple).transpose()
    }

    /// 兑码（票 #57）：原子换 token + 注册码置已用——一次性语义的并发闸，
    /// 且把「未停用 + 未过期」并入条件（防读后写前的 TOCTOU：读到的行在
    /// 兑码瞬间已被停用/过期仍成功换 token）。`WHERE register_code_used = 0`
    /// 保证并发双换只有一个成功（另一个 rows_affected = 0 → 调用侧回 409
    /// 「已使用」）。返回 false 表示兑码被抢（另一请求先兑）、Agent 已停用
    /// 或注册码已过期（调用侧按语义回错）。
    pub async fn redeem_register_code(
        &self,
        id: i64,
        new_token_hash: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE agents
             SET token_hash = ?, register_code_used = 1, updated_at = ?
             WHERE id = ?
               AND register_code_used = 0
               AND disabled = 0
               AND (register_code_expires_at IS NULL OR register_code_expires_at > ?)",
        )
        .bind(new_token_hash)
        .bind(now_ms())
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 全部 Agent（管理面清单；按名排序输出稳定）。
    pub async fn list(&self) -> Result<Vec<AgentRow>, StoreError> {
        let rows = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(AgentRow::from_tuple).collect()
    }

    /// 在线 Agent（心跳超时扫描的输入面；按名排序输出稳定）。
    /// 在线判定：45s 无心跳判离线由扫描侧（`grpc` 面 sweep）以
    /// `last_seen_at` 裁决，本方法只列「当前在线」行。
    pub async fn list_online(&self) -> Result<Vec<AgentRow>, StoreError> {
        let rows = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents WHERE online = 1 ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(AgentRow::from_tuple).collect()
    }

    /// 按行 id 取 Agent；不存在返回 `None`。
    pub async fn get(&self, id: i64) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::from_tuple).transpose()
    }

    /// 按名取 Agent（管理面寻径；不存在返回 `None`）。
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentRowTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, agent_version, upgrade_phase,
                    upgrade_error, pending_upgrade, created_at, updated_at
             FROM agents WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::from_tuple).transpose()
    }

    /// 槽位判定：Agent 在线 + 未停用 + 在途任务（running/unknown）少于
    /// max_concurrency（ADR-0008：Server 端中心化计数，含在途下发）。
    /// 在途计数复用 [`crate::store::jobs::JobRepo::active_by_agent`]——槽位
    /// 语义单点收敛，不另写 SQL。
    pub async fn has_slots(&self, id: i64) -> Result<bool, StoreError> {
        let Some(agent) = self.get(id).await? else {
            return Ok(false);
        };
        if !agent.online || agent.disabled {
            return Ok(false);
        }
        let active = crate::store::jobs::JobRepo::new(self.pool.clone())
            .active_by_agent(id)
            .await?;
        Ok(active < agent.max_concurrency as i64)
    }

    /// 标签 AND 全集匹配：job 要求全部命中即在线候选（ADR-0008 纯 AND
    /// 语义——容器任务调用侧已追加隐式容器标签；本方法不追加）。
    pub fn matches_tags(agent_labels: &[String], required: &[String]) -> bool {
        required
            .iter()
            .all(|tag| agent_labels.iter().any(|a| a == tag))
    }

    /// 为一项 job 的标签要求匹配 Agent（在线、未停用、非升级中、版本兼容、
    /// 有空槽、标签 AND 全集命中，取首个；无匹配返回 `None`——sched 标注
    /// 缺失标签无限等待，ADR-0008）。纯查询面，不落任何调度状态：下发与
    /// 槽位占用由 sched 在 ack 时落。
    pub async fn match_job(&self, required: &[String]) -> Result<Option<i64>, StoreError> {
        for agent in self.list().await? {
            if !agent.dispatchable(&self.server_version)? {
                continue;
            }
            if !self.has_slots(agent.id).await? {
                continue;
            }
            if Self::matches_tags(&agent.all_labels()?, required) {
                return Ok(Some(agent.id));
            }
        }
        Ok(None)
    }

    /// 为一项 job 匹配全部候选 Agent（在线、未停用、非升级中、版本兼容、
    /// 有空槽、标签 AND 全集命中；按名排序输出稳定——sched 匹配面）。
    /// `id` 传入时优先候选（下发失败重试同一 Agent，避免 flutter）。返回
    /// 有序 id 列表（可空——无匹配时 sched 据此标注缺失标签/等待原因）。
    ///
    /// 排空中（`mid_upgrade`）与版本不兼容（`version_incompatible`）Agent 不
    /// 进候选——ADR-0017「在线但不可派发」在调度层兑现：不派新任务，UI 另
    /// 由四态派生展示「排空/版本不兼容」。
    pub async fn match_candidates(
        &self,
        id: Option<i64>,
        required: &[String],
    ) -> Result<Vec<i64>, StoreError> {
        let mut matched = Vec::new();
        let mut preferred = None;
        for agent in self.list().await? {
            if !agent.dispatchable(&self.server_version)? {
                continue;
            }
            if !self.has_slots(agent.id).await? {
                continue;
            }
            if !Self::matches_tags(&agent.all_labels()?, required) {
                continue;
            }
            if Some(agent.id) == id {
                preferred = Some(agent.id);
            } else {
                matched.push(agent.id);
            }
        }
        if let Some(id) = preferred {
            matched.insert(0, id);
        }
        Ok(matched)
    }

    // -----------------------------------------------------------------------
    // 升级面（票 B5-T4，ADR-0017）：版本落库 / 升级状态 / 待补发指令。
    // 升级指令持久化 + 离线重连补发，与取消指令同机制（ADR-0008）。
    // -----------------------------------------------------------------------

    /// 落库握手版本（连接建立时；ADR-0017 版本进契约）。版本以 JSON 文本列
    /// 存储（与 disk_usage 同形态）。
    pub async fn set_agent_version(&self, id: i64, version: &AgentVersion) -> Result<bool, StoreError> {
        let json = serde_json::to_string(version).map_err(StoreError::DefinitionJson)?;
        let result =
            sqlx::query("UPDATE agents SET agent_version = ?, updated_at = ? WHERE id = ?")
                .bind(json)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 落库升级阶段（`UpgradeStatus` 上报时；phase 为 draining/downloading/
    /// swapping/restarting）。`error` 为失败原因（仅 fallback 路径有值，其余
    /// 传 None 保留旧值——`COALESCE` 不清空）。返回 false 表示 Agent 不存在。
    pub async fn set_upgrade_status(
        &self,
        id: i64,
        phase: &str,
        error: Option<&str>,
    ) -> Result<bool, StoreError> {
        let result =
            sqlx::query("UPDATE agents SET upgrade_phase = ?, upgrade_error = COALESCE(?, upgrade_error), updated_at = ? WHERE id = ?")
                .bind(phase)
                .bind(error)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 清升级状态（upgrade_phase/upgrade_error 置空）。升级成功（新进程重连
    /// 后版本已更新）与失败回退（fallback 报告后回到旧版本继续跑）两条路径
    /// 都经此回到「可派发」态。
    pub async fn clear_upgrade_state(&self, id: i64) -> Result<bool, StoreError> {
        let result =
            sqlx::query("UPDATE agents SET upgrade_phase = NULL, upgrade_error = NULL, updated_at = ? WHERE id = ?")
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 记待补发升级指令（管理员下发时；在线即同时下发，离线则重连补发）。
    /// 返回 false 表示 Agent 不存在。
    pub async fn set_pending_upgrade(
        &self,
        id: i64,
        cmd: &PendingUpgrade,
    ) -> Result<bool, StoreError> {
        let json = serde_json::to_string(cmd).map_err(StoreError::DefinitionJson)?;
        let result =
            sqlx::query("UPDATE agents SET pending_upgrade = ?, updated_at = ? WHERE id = ?")
                .bind(json)
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 清待补发升级指令（收到首条 `UpgradeStatus` 回执即清——指令已送达并被
    /// Agent 接收执行，不再补发，避免对正在升级的 Agent 重发非幂等指令）。
    /// 返回 false 表示 Agent 不存在。
    pub async fn clear_pending_upgrade(&self, id: i64) -> Result<bool, StoreError> {
        let result =
            sqlx::query("UPDATE agents SET pending_upgrade = NULL, updated_at = ? WHERE id = ?")
                .bind(now_ms())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 待补发升级指令视图（全库）：`pending_upgrade` 已记但未收 `UpgradeStatus`
    /// 回执（`upgrade_phase` 仍空）的 Agent——重连/启动补发输入面。返回
    /// (agent_id, 指令) 列表（按 id 排序输出稳定）。
    pub async fn pending_upgrade_resend(&self) -> Result<Vec<(i64, PendingUpgrade)>, StoreError> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, pending_upgrade FROM agents
             WHERE pending_upgrade IS NOT NULL AND upgrade_phase IS NULL
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, json)| {
                serde_json::from_str::<PendingUpgrade>(&json)
                    .map(|cmd| (id, cmd))
                    .map_err(StoreError::DefinitionJson)
            })
            .collect()
    }

    /// 某 Agent 的待补发升级指令（重连补发按 Agent 取用）。Agent 不存在或
    /// 无待补发均返回 `None`。
    pub async fn pending_upgrade_for(
        &self,
        id: i64,
    ) -> Result<Option<PendingUpgrade>, StoreError> {
        match self.get(id).await? {
            None => Ok(None),
            Some(row) => row.pending_upgrade(),
        }
    }
}

/// agents 行元组（列形态唯一收敛点，免逐查询散落 `Row::get`）。struct 而非
/// 元组：加列后共 19 列，超 sqlx 的 16 列元组上限（与 jobs `JobRowTuple` 同
/// 纪律）；`#[derive(sqlx::FromRow)]` 按列名映射，SELECT 列序无须与字段序
/// 对齐，但须含全部列。
#[derive(sqlx::FromRow)]
struct AgentRowTuple {
    id: i64,
    name: String,
    token_hash: String,
    system_labels: String,
    custom_labels: String,
    max_concurrency: i32,
    online: bool,
    disabled: bool,
    last_seen_at: Option<i64>,
    disk_usage: Option<String>,
    register_code_hash: String,
    register_code_used: bool,
    register_code_expires_at: Option<i64>,
    agent_version: Option<String>,
    upgrade_phase: Option<String>,
    upgrade_error: Option<String>,
    pending_upgrade: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl AgentRow {
    /// 手工行映射（`AgentRowTuple` → `AgentRow`；列已按名取入 struct，此处
    /// 只搬字段，无 parse 面——`agent_version`/`pending_upgrade` 的 JSON
    /// 解析延后到 [`Self::agent_version`]/[`Self::pending_upgrade`] 按需做）。
    fn from_tuple(row: AgentRowTuple) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            name: row.name,
            token_hash: row.token_hash,
            system_labels: row.system_labels,
            custom_labels: row.custom_labels,
            max_concurrency: row.max_concurrency,
            online: row.online,
            disabled: row.disabled,
            last_seen_at: row.last_seen_at,
            disk_usage: row.disk_usage,
            register_code_hash: row.register_code_hash,
            register_code_used: row.register_code_used,
            register_code_expires_at: row.register_code_expires_at,
            agent_version: row.agent_version,
            upgrade_phase: row.upgrade_phase,
            upgrade_error: row.upgrade_error,
            pending_upgrade: row.pending_upgrade,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::builds::{BuildRepo, StartBuild, TriggerSource};
    use crate::store::jobs::{JobRepo, NewJob};
    use crate::store::projects::{NewProject, ProjectRepo, ScmType};
    use sisyphus_model::pipeline::Revision;
    use sisyphus_model::pipeline::{Job, Pipeline, Shell, Stage, Step};
    use sisyphus_model::validate::BuildSnapshot;

    /// 独立临时目录 + 已迁移库 + 预置项目（store 缝测试形态）。
    async fn fixture() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("临时目录");
        crate::config::Config::load(
            dir.path().to_path_buf(),
            crate::config::Overrides::default(),
            crate::config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = super::super::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        (dir, pool)
    }

    fn new_agent(name: &str, system: &str, custom: &str) -> NewAgent {
        NewAgent {
            name: name.into(),
            token_hash: format!("sisa-hash-{name}"),
            system_labels: system.into(),
            custom_labels: custom.into(),
            max_concurrency: 1,
            register_code_hash: format!("code-hash-{name}"),
            register_code_expires_at: 1_700_000_000_000 + 24 * 60 * 60 * 1000,
        }
    }

    #[tokio::test]
    async fn create_list_get_and_unique_name() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());

        let a = repo
            .create(new_agent(
                "linux-1",
                r#"["sisyphus/os=linux","sisyphus/arch=amd64"]"#,
                r#"["region=cn"]"#,
            ))
            .await
            .expect("建条目");
        assert!(a.id > 0);
        assert!(!a.online && !a.disabled);
        assert_eq!(a.max_concurrency, 1);
        assert!(a.last_seen_at.is_none(), "从未在线");
        assert_eq!(a.register_code_hash, "code-hash-linux-1");

        // 名称撞唯一约束。
        let err = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect_err("重名应拒绝");
        assert!(matches!(err, StoreError::Unique(_)));

        // 按名/按 id 读回等价。
        let by_id = repo.get(a.id).await.expect("查").expect("应存在");
        let by_name = repo
            .get_by_name("linux-1")
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(by_id, by_name);
        assert_eq!(
            by_name.all_labels().expect("标签"),
            vec![
                "sisyphus/os=linux".to_string(),
                "sisyphus/arch=amd64".to_string(),
                "region=cn".to_string(),
            ],
            "系统 + 自定义取并集"
        );

        let list = repo.list().await.expect("清单");
        assert_eq!(list.len(), 1);
        assert!(repo.get_by_name("nope").await.expect("查").is_none());
    }

    /// 票 #57 AC（store 缝）：注册码换 token 的查行面 + 原子兑码——
    /// 未兑可换（token 换新、置已用）、重兑返回 false（并发闸）、无效码
    /// None、有效期字段随建条目落定。
    #[tokio::test]
    async fn register_code_redeem_is_one_time_and_rotates_token() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建条目");
        assert!(!agent.register_code_used, "新建未兑");
        assert_eq!(
            agent.register_code_expires_at,
            Some(1_700_000_000_000 + 24 * 60 * 60 * 1000),
            "建条目即签 24h 有效期"
        );

        // 无效码：None。
        assert!(
            repo.find_by_register_code("code-hash-unknown")
                .await
                .expect("查")
                .is_none()
        );

        // 未兑可换：查行 → 原子兑码 → token 换新、置已用。
        let row = repo
            .find_by_register_code("code-hash-linux-1")
            .await
            .expect("查")
            .expect("应存在");
        assert_eq!(row.id, agent.id);
        assert!(
            repo.redeem_register_code(row.id, "sisa-hash-new", 1_700_000_000_000)
                .await
                .expect("兑码")
        );
        let after = repo.get(agent.id).await.expect("查").expect("应存在");
        assert!(after.register_code_used, "兑码后置已用");
        assert_eq!(after.token_hash, "sisa-hash-new", "token 换新");

        // 重兑：false（一次性防重放——并发双换的败者也走这条路）。
        assert!(
            !repo
                .redeem_register_code(agent.id, "sisa-hash-another", 1_700_000_000_000)
                .await
                .expect("重兑应 false")
        );

        // 过期码：false（原子闸含「未过期」条件——读后写前的 TOCTOU 关闭）。
        let expired = repo
            .create(new_agent("linux-2", "[]", "[]"))
            .await
            .expect("建 linux-2");
        sqlx::query("UPDATE agents SET register_code_expires_at = 100 WHERE id = ?")
            .bind(expired.id)
            .execute(&pool)
            .await
            .expect("拨过期");
        assert!(
            !repo
                .redeem_register_code(expired.id, "sisa-hash-expired", 1_000)
                .await
                .expect("过期应 false")
        );

        // 停用：false（原子闸含「未停用」条件）。
        let disabled = repo
            .create(new_agent("linux-3", "[]", "[]"))
            .await
            .expect("建 linux-3");
        repo.set_disabled(disabled.id, true).await.expect("停用");
        assert!(
            !repo
                .redeem_register_code(disabled.id, "sisa-hash-disabled", 1_000)
                .await
                .expect("停用应 false")
        );

        // 兑码后 token 认证面以新哈希命中、旧哈希失效。
        assert!(
            repo.find_active_by_hash("sisa-hash-new")
                .await
                .expect("认证")
                .is_some()
        );
        assert!(
            repo.find_active_by_hash("sisa-hash-linux-1")
                .await
                .expect("认证")
                .is_none(),
            "兑码即吊销旧 token（换新）"
        );
    }

    #[tokio::test]
    async fn lifecycle_online_offline_disable_and_spec_edit() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");

        // 上线：刷 last_seen、置 online、系统标签与建议并发更新。
        assert!(
            repo.mark_online(
                agent.id,
                r#"["sisyphus/os=linux","sisyphus/container=docker"]"#,
                Some(4),
                1_000,
            )
            .await
            .expect("上线")
        );
        let up = repo.get(agent.id).await.expect("查").expect("应存在");
        assert!(up.online);
        assert_eq!(up.last_seen_at, Some(1_000));
        assert_eq!(up.max_concurrency, 4, "注册上报建议值作初始值");
        assert!(up.system_labels.contains("sisyphus/container=docker"));

        // 心跳再刷新（建议并发 None 不动现有值）。
        assert!(
            repo.mark_online(agent.id, up.system_labels.as_str(), None, 2_000)
                .await
                .expect("心跳")
        );
        assert_eq!(
            repo.get(agent.id)
                .await
                .expect("查")
                .expect("应存在")
                .last_seen_at,
            Some(2_000)
        );

        // 离线：online 置 0、last_seen 刷新（运行中任务处置归 sched）。
        assert!(repo.mark_offline(agent.id, 3_000).await.expect("离线"));
        let down = repo.get(agent.id).await.expect("查").expect("应存在");
        assert!(!down.online);
        assert_eq!(down.last_seen_at, Some(3_000));

        // 编辑槽位与自定义标签。
        assert!(
            repo.update_spec(agent.id, 2, r#"["region=eu"]"#)
                .await
                .expect("编辑")
        );
        let edited = repo.get(agent.id).await.expect("查").expect("应存在");
        assert_eq!(edited.max_concurrency, 2);
        assert!(edited.custom_labels.contains("region=eu"));

        // 停用：踢线（认证面不命中）。
        assert!(repo.set_disabled(agent.id, true).await.expect("停用"));
        assert!(
            repo.find_active_by_hash("sisa-hash-linux-1")
                .await
                .expect("认证查")
                .is_none(),
            "停用即踢线"
        );
        // 启用后恢复认证。
        assert!(repo.set_disabled(agent.id, false).await.expect("启用"));
        assert!(
            repo.find_active_by_hash("sisa-hash-linux-1")
                .await
                .expect("认证查")
                .is_some()
        );
        assert!(
            repo.find_active_by_hash("sisa-hash-unknown")
                .await
                .expect("认证查")
                .is_none(),
            "未知哈希一律 None"
        );
    }

    #[tokio::test]
    async fn has_slots_counts_inflight_tasks_until_terminal() {
        let (_dir, pool) = fixture().await;
        let agents = AgentRepo::new(pool.clone());
        let jobs = JobRepo::new(pool.clone());
        let agent = agents
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");
        agents
            .mark_online(agent.id, "[]", None, 1_000)
            .await
            .expect("上线");

        assert!(agents.has_slots(agent.id).await.expect("槽位"), "空槽");

        // 建一个 queued 构建下两任务，调度到本 Agent。
        let project = ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: None,
            })
            .await
            .expect("建项目");
        let pipeline = Pipeline {
            name: "build".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec![],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "cargo build".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        };
        let build = BuildRepo::new(pool.clone())
            .start(StartBuild {
                project_id: project.id,
                pipeline_name: "build".into(),
                trigger: TriggerSource::Manual,
                trigger_detail: "{}".into(),
                snapshot: BuildSnapshot::new(
                    pipeline,
                    Revision {
                        number: 1,
                        operator: "tester".into(),
                        at_ms: 1_000,
                    },
                ),
            })
            .await
            .expect("开始构建");
        let j1 = jobs
            .insert(NewJob {
                build_id: build.id,
                stage_index: 0,
                name: "j1".into(),
                attempt: 1,
                spec_json: None,
                agent_id: Some(agent.id),
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j1");
        let j2 = jobs
            .insert(NewJob {
                build_id: build.id,
                stage_index: 0,
                name: "j2".into(),
                attempt: 1,
                spec_json: None,
                agent_id: Some(agent.id),
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j2");

        // queued 不占槽；一个 running 占满单槽。
        jobs.transition(
            j1.id,
            crate::store::jobs::JobStatus::Running,
            None,
            None,
            1_000,
        )
        .await
        .expect("j1 运行");
        assert!(!agents.has_slots(agent.id).await.expect("槽位"), "单槽被占");

        // 终态释放槽位。
        jobs.transition(
            j1.id,
            crate::store::jobs::JobStatus::Succeeded,
            Some(0),
            None,
            2_000,
        )
        .await
        .expect("j1 完成");
        assert!(agents.has_slots(agent.id).await.expect("槽位"), "终态释放");

        // 离线/停用：无槽（即使有空位也不接新任务）。
        agents.mark_offline(agent.id, 3_000).await.expect("离线");
        assert!(!agents.has_slots(agent.id).await.expect("槽位"));
        agents
            .mark_online(agent.id, "[]", None, 4_000)
            .await
            .expect("上线");
        agents.set_disabled(agent.id, true).await.expect("停用");
        assert!(!agents.has_slots(agent.id).await.expect("槽位"));
        agents.set_disabled(agent.id, false).await.expect("启用");

        // 不存在的 Agent：无槽。
        assert!(!agents.has_slots(agent.id + 999).await.expect("槽位"));
        let _ = j2;
    }

    /// 票 #45 AC：标签 AND 全集匹配（含隐式容器标签的显式形态）——
    /// 纯标签匹配判据单测。
    #[test]
    fn matches_tags_is_and_semantics() {
        let agent = vec![
            "sisyphus/os=linux".to_string(),
            "sisyphus/arch=amd64".to_string(),
            "sisyphus/container=docker".to_string(),
        ];
        // 全部命中。
        assert!(AgentRepo::matches_tags(
            &agent,
            &[
                "sisyphus/os=linux".to_string(),
                "sisyphus/arch=amd64".to_string()
            ],
        ));
        // 容器任务（调用侧已追加隐式容器标签）命中。
        assert!(AgentRepo::matches_tags(
            &agent,
            &[
                "sisyphus/os=linux".to_string(),
                "sisyphus/container=docker".to_string(),
            ],
        ));
        // 缺一个即不命中（AND 全集，不是 OR）。
        assert!(!AgentRepo::matches_tags(
            &agent,
            &["sisyphus/os=windows".to_string()],
        ));
        // 空要求：任意 Agent 命中（无标签约束任务）。
        assert!(AgentRepo::matches_tags(&agent, &[]));
        assert!(AgentRepo::matches_tags(&[], &[]));
    }

    #[tokio::test]
    async fn heartbeat_persists_disk_usage_labels_and_respects_disabled() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");

        // 心跳：置在线、刷 last_seen、整组替换系统标签、落磁盘占用。
        let disk = r#"{"volumes":[{"mount_point":"/","total_bytes":100,"free_bytes":40}],"cache_bytes":5,"workspace_bytes":10}"#;
        let ok = repo
            .heartbeat(
                agent.id,
                r#"["sisyphus/os=linux","sisyphus/arch=amd64","sisyphus/container=docker"]"#,
                Some(disk),
                1_000,
            )
            .await
            .expect("心跳");
        assert!(ok);
        let row = repo.get(agent.id).await.expect("查").expect("应存在");
        assert!(row.online);
        assert_eq!(row.last_seen_at, Some(1_000));
        assert!(row.system_labels.contains("sisyphus/container=docker"));
        assert_eq!(
            row.disk_usage().expect("磁盘占用").expect("应上报"),
            AgentDiskUsage {
                volumes: vec![VolumeUsage {
                    mount_point: "/".into(),
                    total_bytes: 100,
                    free_bytes: 40,
                }],
                cache_bytes: 5,
                workspace_bytes: 10,
            }
        );

        // 再次心跳不带上报：在线/标签刷新、旧磁盘占用保留。
        assert!(
            repo.heartbeat(agent.id, r#"["sisyphus/os=linux"]"#, None, 2_000)
                .await
                .expect("再心跳")
        );
        let row = repo.get(agent.id).await.expect("查").expect("应存在");
        assert_eq!(row.last_seen_at, Some(2_000));
        assert_eq!(row.system_labels, r#"["sisyphus/os=linux"]"#);
        assert!(
            row.disk_usage().expect("磁盘占用").is_some(),
            "None 上报不清旧值"
        );

        // 停用即踢线：停用 Agent 的心跳不生效（在线面立即拒——通道据此断开，
        // 不等待下次连接受拒）。
        assert!(repo.set_disabled(agent.id, true).await.expect("停用"));
        assert!(
            !repo
                .heartbeat(agent.id, "[]", None, 3_000)
                .await
                .expect("停用心跳"),
            "停用心跳应不生效"
        );
        let row = repo.get(agent.id).await.expect("查").expect("应存在");
        assert_eq!(row.last_seen_at, Some(2_000), "停用心跳不刷 last_seen");

        // 启用后心跳恢复生效。
        assert!(repo.set_disabled(agent.id, false).await.expect("启用"));
        assert!(
            repo.heartbeat(agent.id, "[]", None, 4_000)
                .await
                .expect("启用心跳")
        );

        // 不存在的 Agent：false。
        assert!(
            !repo
                .heartbeat(agent.id + 999, "[]", None, 5_000)
                .await
                .expect("不存在")
        );
    }

    #[tokio::test]
    async fn list_online_filters_by_online_flag() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let a = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("a");
        repo.create(new_agent("linux-2", "[]", "[]"))
            .await
            .expect("b");

        assert!(repo.list_online().await.expect("在线清单").is_empty());
        repo.heartbeat(a.id, "[]", None, 1_000)
            .await
            .expect("a 上线");
        let online = repo.list_online().await.expect("在线清单");
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].name, "linux-1");
    }

    /// 票 #45 AC：匹配查询——在线 + 空槽 + 标签 AND 全集命中才下发候选；
    /// 离线/停用/无槽/缺标签都不命中。
    #[tokio::test]
    async fn match_job_respects_online_slots_and_tag_and_semantics() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let linux = repo
            .create(new_agent(
                "linux-1",
                r#"["sisyphus/os=linux","sisyphus/arch=amd64","sisyphus/container=docker"]"#,
                r#"["region=cn"]"#,
            ))
            .await
            .expect("linux");
        repo.mark_online(linux.id, linux.system_labels.as_str(), None, 1_000)
            .await
            .expect("上线");

        // 无标签约束任务：命中（空要求 AND 恒真）。
        assert_eq!(repo.match_job(&[]).await.expect("匹配"), Some(linux.id));

        // 系统标签 + 自定义标签要求都命中（AND 全集跨两层标签）。
        let required = vec![
            "sisyphus/os=linux".to_string(),
            "sisyphus/container=docker".to_string(),
            "region=cn".to_string(),
        ];
        assert_eq!(
            repo.match_job(&required).await.expect("匹配"),
            Some(linux.id)
        );

        // 缺任一标签：无匹配（无限等待，sched 标注缺失标签）。
        let missing = vec!["sisyphus/os=linux".to_string(), "gpu=nvidia".to_string()];
        assert_eq!(repo.match_job(&missing).await.expect("匹配"), None);

        // 离线：不接新任务（标签全命中也不行）。
        repo.mark_offline(linux.id, 2_000).await.expect("离线");
        assert_eq!(repo.match_job(&required).await.expect("匹配"), None);

        // 在线后无槽：不命中。
        repo.mark_online(linux.id, linux.system_labels.as_str(), None, 3_000)
            .await
            .expect("上线");
        assert_eq!(linux.max_concurrency, 1, "单槽");
        // 借 jobs 表占满唯一槽位（在线 + 单槽 + 一任务 running）。
        let project = ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url: "https://example.com/repo".into(),
                default_branch: None,
            })
            .await
            .expect("建项目");
        let pipeline = Pipeline {
            name: "build".into(),
            parameters: vec![],
            env: vec![],
            notification: None,
            stages: vec![Stage {
                name: "build".into(),
                when: None,
                jobs: vec![Job {
                    name: "compile".into(),
                    exec_env: None,
                    labels: vec![],
                    when: None,
                    env: vec![],
                    allow_failure: false,
                    retry_count: 0,
                    timeout_minutes: 0,
                    artifact_uploads: vec![],
                    artifact_downloads: vec![],
                    caches: vec![],
                    secrets: vec![],
                    steps: vec![Step::Shell {
                        command: "cargo build".into(),
                        shell: Some(Shell::Bash),
                        when: None,
                    }],
                }],
            }],
            revision: None,
        };
        let build = BuildRepo::new(pool.clone())
            .start(StartBuild {
                project_id: project.id,
                pipeline_name: "build".into(),
                trigger: TriggerSource::Manual,
                trigger_detail: "{}".into(),
                snapshot: BuildSnapshot::new(
                    pipeline,
                    Revision {
                        number: 1,
                        operator: "tester".into(),
                        at_ms: 1_000,
                    },
                ),
            })
            .await
            .expect("开始构建");
        let j1 = JobRepo::new(pool.clone())
            .insert(NewJob {
                build_id: build.id,
                stage_index: 0,
                name: "j1".into(),
                attempt: 1,
                spec_json: None,
                agent_id: Some(linux.id),
                labels: vec![],
                timeout_minutes: 0,
                retry_count: 0,
                allow_failure: false,
            })
            .await
            .expect("j1");
        JobRepo::new(pool.clone())
            .transition(
                j1.id,
                crate::store::jobs::JobStatus::Running,
                None,
                None,
                4_000,
            )
            .await
            .expect("j1 运行");
        assert_eq!(
            repo.match_job(&required).await.expect("匹配"),
            None,
            "无空槽不命中"
        );
        // 终态释放后恢复命中。
        JobRepo::new(pool.clone())
            .transition(
                j1.id,
                crate::store::jobs::JobStatus::Succeeded,
                Some(0),
                None,
                5_000,
            )
            .await
            .expect("j1 完成");
        assert_eq!(
            repo.match_job(&required).await.expect("匹配"),
            Some(linux.id)
        );

        // 停用：不命中；启用恢复。
        repo.set_disabled(linux.id, true).await.expect("停用");
        assert_eq!(repo.match_job(&required).await.expect("匹配"), None);
        repo.set_disabled(linux.id, false).await.expect("启用");
        assert_eq!(
            repo.match_job(&required).await.expect("匹配"),
            Some(linux.id)
        );

        // 无任何 Agent 的库：None（先清引用它的 jobs 再清 agents——外键）。
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .expect("清 jobs");
        sqlx::query("DELETE FROM agents")
            .execute(&pool)
            .await
            .expect("清 agents");
        assert_eq!(repo.match_job(&[]).await.expect("匹配"), None);
    }

    /// 票 B2c-T4：候选匹配面——`match_candidates` 返回全部候选（在线 + 空槽 +
    /// 标签 AND），`id` 偏好优先；sched 下发失败重试同一 Agent 用。
    #[tokio::test]
    async fn match_candidates_returns_ordered_preferred_first() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let linux = repo
            .create(new_agent("linux-1", r#"["sisyphus/os=linux"]"#, "[]"))
            .await
            .expect("linux");
        let mac = repo
            .create(new_agent("mac-1", r#"["sisyphus/os=macos"]"#, "[]"))
            .await
            .expect("mac");
        repo.mark_online(linux.id, linux.system_labels.as_str(), None, 1_000)
            .await
            .expect("linux 上线");
        repo.mark_online(mac.id, mac.system_labels.as_str(), None, 1_000)
            .await
            .expect("mac 上线");

        let linux_req = vec!["sisyphus/os=linux".to_string()];
        assert_eq!(
            repo.match_candidates(None, &linux_req).await.expect("候选"),
            vec![linux.id],
            "只有 linux 命中"
        );
        // id 偏好：首候选优先（即使在线清单里排后）。
        assert_eq!(
            repo.match_candidates(Some(linux.id), &[])
                .await
                .expect("候选"),
            vec![linux.id, mac.id],
            "无标签约束：两者候选，偏好者排前"
        );

        // 离线/停用/无槽过滤照旧（候选集不包含不可调度者）。
        repo.mark_offline(mac.id, 2_000).await.expect("mac 离线");
        assert_eq!(
            repo.match_candidates(None, &[]).await.expect("候选"),
            vec![linux.id],
            "离线 Agent 不在候选"
        );
    }

    // -----------------------------------------------------------------------
    // 升级面（票 B5-T4 / #76，ADR-0017）：版本落库 / 升级状态 / 待补发指令 /
    // 排空与版本不兼容的派发门。
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn agent_version_round_trips_as_json() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");
        // 新建从未握手：版本为空。
        assert!(
            repo.get(agent.id).await.unwrap().unwrap().agent_version.is_none(),
            "新建 Agent 无版本"
        );

        // 落 1.0.0 → 读回等价。
        let v100 = AgentVersion {
            major: 1,
            minor: 0,
            patch: 0,
        };
        assert!(
            repo.set_agent_version(agent.id, &v100).await.expect("落版本")
        );
        assert_eq!(
            repo.get(agent.id).await.unwrap().unwrap().agent_version().unwrap(),
            Some(v100)
        );

        // 再落 0.9.5（升级/降级回退后）覆盖。
        let v095 = AgentVersion {
            major: 0,
            minor: 9,
            patch: 5,
        };
        repo.set_agent_version(agent.id, &v095).await.expect("覆盖版本");
        assert_eq!(
            repo.get(agent.id).await.unwrap().unwrap().agent_version().unwrap(),
            Some(v095)
        );

        // 不存在的 Agent：false。
        assert!(
            !repo
                .set_agent_version(agent.id + 999, &v100)
                .await
                .expect("不存在")
        );
    }

    #[tokio::test]
    async fn upgrade_status_set_clear_and_error_preserved() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");

        // 排空阶段落定。
        repo.set_upgrade_status(agent.id, "draining", None)
            .await
            .expect("排空");
        let row = repo.get(agent.id).await.unwrap().unwrap();
        assert_eq!(row.upgrade_phase.as_deref(), Some("draining"));
        assert!(row.upgrade_error.is_none());

        // 下载阶段带错误（模拟下载失败：phase 仍 downloading，error 记因）。
        repo.set_upgrade_status(agent.id, "downloading", Some("下载失败：超时"))
            .await
            .expect("下载");
        let row = repo.get(agent.id).await.unwrap().unwrap();
        assert_eq!(row.upgrade_phase.as_deref(), Some("downloading"));
        assert_eq!(row.upgrade_error.as_deref(), Some("下载失败：超时"));

        // 再次上报不带 error：COALESCE 保留旧 error（不清空）。
        repo.set_upgrade_status(agent.id, "downloading", None)
            .await
            .expect("再报");
        let row = repo.get(agent.id).await.unwrap().unwrap();
        assert_eq!(
            row.upgrade_error.as_deref(),
            Some("下载失败：超时"),
            "None 上报不清旧 error"
        );

        // 清升级状态：phase/error 双空。
        repo.clear_upgrade_state(agent.id).await.expect("清");
        let row = repo.get(agent.id).await.unwrap().unwrap();
        assert!(row.upgrade_phase.is_none() && row.upgrade_error.is_none());
    }

    /// 票 #76 AC：升级指令持久化 + 离线补发视图——待补发指令落库可读，收到
    /// 首条 UpgradeStatus 回执（upgrade_phase 置位）即移出补发视图，clear
    /// 清空。
    #[tokio::test]
    async fn pending_upgrade_resend_view_tracks_until_acked() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let a = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("a");
        let b = repo
            .create(new_agent("linux-2", "[]", "[]"))
            .await
            .expect("b");

        let cmd = PendingUpgrade {
            package_name: "sisyphus-agent-1.0.0-linux-x86_64.tar.gz".into(),
            sha256: "abc123".into(),
            download_url: "/api/v1/agent/upgrade-packages/sisyphus-agent-1.0.0-linux-x86_64.tar.gz".into(),
        };

        // 两条待补发：补发视图含 a、b（按 id 排序）。
        repo.set_pending_upgrade(a.id, &cmd).await.expect("a 待补发");
        repo.set_pending_upgrade(b.id, &cmd).await.expect("b 待补发");
        let view = repo.pending_upgrade_resend().await.expect("补发视图");
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].0, a.id);
        assert_eq!(view[1].0, b.id);
        assert_eq!(view[0].1, cmd);

        // 收到 a 的首条回执（upgrade_phase 置位）→ a 移出补发视图（不再重发，
        // 避免对正在升级的 Agent 重发非幂等指令）。
        repo.set_upgrade_status(a.id, "draining", None)
            .await
            .expect("a 收到回执");
        let view = repo.pending_upgrade_resend().await.expect("补发视图");
        assert_eq!(view.len(), 1, "a 已回执不再补发");
        assert_eq!(view[0].0, b.id);

        // 单 Agent 取待补发：b 有、a 有 pending 但 phase 已置——按 Agent 取仍读得到
        // pending（pending_upgrade_for 读列原值，不过滤 phase）。
        assert!(
            repo.pending_upgrade_for(a.id).await.unwrap().is_some(),
            "pending_upgrade_for 读列原值（phase 已置也仍在列里，直到显式 clear）"
        );

        // 清 b 的待补发：补发视图空。
        repo.clear_pending_upgrade(b.id).await.expect("清 b");
        assert!(repo.pending_upgrade_resend().await.unwrap().is_empty());

        // 清 a 的待补发 + 清升级状态：彻底干净。
        repo.clear_pending_upgrade(a.id).await.expect("清 a");
        repo.clear_upgrade_state(a.id).await.expect("a 清状态");
        assert!(
            repo.get(a.id).await.unwrap().unwrap().pending_upgrade.is_none()
        );
    }

    /// 票 #76 AC：排空中 Agent 不接新任务（调度断言）——upgrade_phase=draining
    /// 或 pending_upgrade 已记的在线 Agent 不进 match_candidates 候选。
    #[tokio::test]
    async fn match_candidates_skips_draining_agent() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let agent = repo
            .create(new_agent("linux-1", "[]", "[]"))
            .await
            .expect("建");
        repo.mark_online(agent.id, "[]", None, 1_000).await.expect("上线");

        // 在线 + 无标签约束：本应命中。
        assert_eq!(
            repo.match_candidates(None, &[]).await.unwrap(),
            vec![agent.id],
            "基线：在线可派发"
        );

        // 排空阶段：不进候选（在线但不可派发）。
        repo.set_upgrade_status(agent.id, "draining", None)
            .await
            .expect("排空");
        assert!(
            repo.match_candidates(None, &[]).await.unwrap().is_empty(),
            "排空中 Agent 不接新任务"
        );

        // 有待补发指令（pending 已记、phase 仍空）：同样不进候选（即将排空）。
        repo.clear_upgrade_state(agent.id).await.expect("清状态");
        repo.set_pending_upgrade(
            agent.id,
            &PendingUpgrade {
                package_name: "p".into(),
                sha256: "s".into(),
                download_url: "/u".into(),
            },
        )
        .await
        .expect("待补发");
        assert!(
            repo.match_candidates(None, &[]).await.unwrap().is_empty(),
            "有待补发升级指令的 Agent 不接新任务"
        );

        // fallback 阶段：回到可派发（升级失败回退旧版本继续跑）。
        repo.clear_pending_upgrade(agent.id).await.expect("清待补发");
        repo.set_upgrade_status(agent.id, "fallback", Some("退回 .old"))
            .await
            .expect("回退");
        assert_eq!(
            repo.match_candidates(None, &[]).await.unwrap(),
            vec![agent.id],
            "fallback 后恢复可派发"
        );
    }

    /// 票 #76 AC：过旧 Agent 任务面拒连（无派发）+ 升级面保留——
    /// agent_version < N-1（0.8.0 vs Server 1.0.0）的在线 Agent 不进候选；
    /// 窗口内（0.9.0 / 1.0.0）正常派发。版本不兼容 ≠ 离线（dispatchable 据此
    /// 区分，UI 四态派生消费）。
    #[tokio::test]
    async fn match_candidates_skips_version_incompatible_agent() {
        let (_dir, pool) = fixture().await;
        let repo = AgentRepo::new(pool.clone());
        let server = repo.server_version();
        assert_eq!(server, AgentVersion {
            major: 1,
            minor: 0,
            patch: 0
        });

        let too_old = repo.create(new_agent("old-1", "[]", "[]")).await.expect("old");
        let in_window = repo.create(new_agent("ok-1", "[]", "[]")).await.expect("ok");
        for a in [&too_old, &in_window] {
            repo.mark_online(a.id, "[]", None, 1_000).await.expect("上线");
        }
        repo.set_agent_version(too_old.id, &AgentVersion { major: 0, minor: 8, patch: 0 })
            .await
            .expect("落旧版本");
        repo.set_agent_version(in_window.id, &AgentVersion { major: 0, minor: 9, patch: 0 })
            .await
            .expect("落窗口内版本");

        // 派生谓词：过旧 Agent 在线但 dispatchable=false（版本不兼容，非离线）；
        // 窗口内 Agent dispatchable=true。
        let old_row = repo.get(too_old.id).await.unwrap().unwrap();
        let ok_row = repo.get(in_window.id).await.unwrap().unwrap();
        assert!(old_row.online, "过旧 Agent 仍在线");
        assert!(!old_row.dispatchable(&server).unwrap(), "过旧 Agent 不可派发");
        assert!(old_row.version_incompatible(&server).unwrap());
        assert!(!ok_row.version_incompatible(&server).unwrap(), "0.9.0 在窗口内");
        assert!(ok_row.dispatchable(&server).unwrap());

        // 候选只含窗口内 Agent（过旧者被版本门滤掉）。
        assert_eq!(
            repo.match_candidates(None, &[]).await.unwrap(),
            vec![in_window.id],
            "过旧 Agent 不进候选、窗口内 Agent 进"
        );

        // 升级面保留契约点：过旧 Agent 仍可连（compatible 只判过新）——
        // 此处用 too_old 不被 match 命中、但行仍在线/可管理作可观察侧证。
        assert!(repo.get(too_old.id).await.unwrap().unwrap().online);
    }
}

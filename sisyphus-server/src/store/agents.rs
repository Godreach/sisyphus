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
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

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

/// Agent repo：建条目 / 启停与编辑 / 在线维护 / 标签匹配。
#[derive(Debug, Clone)]
pub struct AgentRepo {
    pool: SqlitePool,
}

impl AgentRepo {
    /// 以连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
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
        let row = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
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
        let row = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
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
        let rows = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
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
        let rows = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
             FROM agents WHERE online = 1 ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(AgentRow::from_tuple).collect()
    }

    /// 按行 id 取 Agent；不存在返回 `None`。
    pub async fn get(&self, id: i64) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
             FROM agents WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::from_tuple).transpose()
    }

    /// 按名取 Agent（管理面寻径；不存在返回 `None`）。
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AgentRow>, StoreError> {
        let row = sqlx::query_as::<_, AgentTuple>(
            "SELECT id, name, token_hash, system_labels, custom_labels, max_concurrency,
                    online, disabled, last_seen_at, disk_usage, register_code_hash,
                    register_code_used, register_code_expires_at, created_at, updated_at
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

    /// 为一项 job 的标签要求匹配 Agent（在线 + 未停用 + 有空槽 + 标签 AND
    /// 全集命中，取首个；无匹配返回 `None`——sched 标注缺失标签无限等待，
    /// ADR-0008）。纯查询面，不落任何调度状态：下发与槽位占用由 sched
    /// 在 ack 时落。
    pub async fn match_job(&self, required: &[String]) -> Result<Option<i64>, StoreError> {
        for agent in self.list().await? {
            if !agent.online || agent.disabled {
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

    /// 为一项 job 匹配全部候选 Agent（在线 + 未停用 + 有空槽 + 标签 AND
    /// 全集命中；按名排序输出稳定——sched 匹配面）。`id` 传入时优先候选
    /// （下发失败重试同一 Agent，避免 flutter）。返回有序 id 列表（可空——
    /// 无匹配时 sched 据此标注缺失标签/等待原因）。
    pub async fn match_candidates(
        &self,
        id: Option<i64>,
        required: &[String],
    ) -> Result<Vec<i64>, StoreError> {
        let mut matched = Vec::new();
        let mut preferred = None;
        for agent in self.list().await? {
            if !agent.online || agent.disabled {
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
}

/// agents 行元组（列形态唯一收敛点，免逐查询散落 `Row::get`）。
type AgentTuple = (
    i64,            // id
    String,         // name
    String,         // token_hash
    String,         // system_labels
    String,         // custom_labels
    i32,            // max_concurrency
    bool,           // online
    bool,           // disabled
    Option<i64>,    // last_seen_at
    Option<String>, // disk_usage
    String,         // register_code_hash
    bool,           // register_code_used
    Option<i64>,    // register_code_expires_at
    i64,            // created_at
    i64,            // updated_at
);

impl AgentRow {
    /// 手工行映射（未知取值不在本行形态内，无 parse 面；返回 `Result`
    /// 与其他 repo 行映射同形，便于 `?` 链式收集）。
    fn from_tuple(row: AgentTuple) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.0,
            name: row.1,
            token_hash: row.2,
            system_labels: row.3,
            custom_labels: row.4,
            max_concurrency: row.5,
            online: row.6,
            disabled: row.7,
            last_seen_at: row.8,
            disk_usage: row.9,
            register_code_hash: row.10,
            register_code_used: row.11,
            register_code_expires_at: row.12,
            created_at: row.13,
            updated_at: row.14,
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
}

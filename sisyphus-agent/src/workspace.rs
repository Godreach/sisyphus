//! Agent 侧工作区管理（ADR-0011；票 B3-T4 / #58）。
//!
//! 工作区 = 单任务的执行目录：`<工作区根>/<pipeline>/<job>/`。同 job 的再次
//! 构建与从失败任务重跑复用同一工作区（ADR-0006）；job 改名即新工作区。本
//! 模块负责：
//!
//! - **布局与清洗**：`<根>/<pipeline>/<job>/` 定位；目录名清洗（非
//!   `[A-Za-z0-9._-]` 替换 `_`，`.`/`..` 归一为 `_`，超长截断）；同名清洗
//!   冲突追加 `-<N>` id 后缀。每级目录写一个 `.sisyphus-ws*.json` 标识标记，
//!   记录原始 pipeline/job 名（清洗不可逆，列表/清理据此还原真名）与最近
//!   使用时间——不做本地锁文件（ADR-0011：锁文件被强杀后反成脏状态来源）。
//! - **列表/清理指令**：`WorkspaceList`（遍历根两层还原 (pipeline, job,
//!   path, last_used)）/`WorkspaceClean`（单 job / 单 pipeline / 全清，删树
//!   严格限定在工作区根下，永不触碰缓存根——缓存根是 `<data>/cache/`，
//!   与工作区根互为独立目录，清理只 `remove_dir_all` 标记定位到的子树）。
//! - **残留检查**：内存级「运行中 job 集合」（[`RunningJobs`]）。重跑下发
//!   时同 job 已在跑 → [`RunningJobs::claim`] 返回 `false` → 拒收；不做锁
//!   文件。runner（#59）在收 JobSpec 前 claim、终态时 release。
//! - **工作区占用采样**：[`WorkspaceSampler`] 后台低频遍历根求和（默认
//!   [`DEFAULT_WORKSPACE_SAMPLE_INTERVAL`] 10 分钟，间隔可注入避免真实
//!   sleep），最近值存原子变量，供心跳 [`DiskUsage`] 附带（经
//!   [`WorkspaceUsage`] trait）。spawn 时先采样一次——心跳首帧即有值。
//! - **`${SISY_WORKSPACE}` 占位替换**：[`expand_sisy_workspace`] 在 runner
//!   （#59）执行任何步骤前把占位符替换为 job 工作区绝对路径（ADR-0006/0011
//!   的变量解析补丁：7 个内置变量 Server 端解析，`SISY_WORKSPACE` 唯一
//!   Agent 端解析）；`$${SISY_WORKSPACE}` 转义为字面量，与 Server 端 `${}`
//!   转义纪律一致。
//!
//! 上行：列表响应经 [`Workspace::set_live`] 注入的活体发送器送出（与 logbuf
//! 同款：连接期 `run_connection` 注入 `out_tx`，断线置 `None`）。单 writer 保
//! 写序——响应帧与心跳/日志同一上行通道。

use std::collections::HashSet;
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sisyphus_proto::agent::{
    ChannelMessage, WorkspaceCommand, WorkspaceEntry, WorkspaceList, channel_message::Kind,
    workspace_command::Kind as WorkspaceKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::ReceiptLog;

/// 工作区占用采样默认间隔（ADR-0011/0019：低频后台遍历，默认 10 分钟）。
pub const DEFAULT_WORKSPACE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// `${SISY_WORKSPACE}` 占位符（ADR-0006 内置变量，Agent 端替换）。
pub const PLACEHOLDER: &str = "${SISY_WORKSPACE}";
/// `$${SISY_WORKSPACE}` 转义（与 Server 端 `${}` 转义纪律一致：输出字面量）。
pub const PLACEHOLDER_ESCAPED: &str = "$${SISY_WORKSPACE}";
/// job 目录标识标记文件名前缀（记录原始 pipeline/job 名 + 最近使用时间）。
/// 标记是 pipeline 目录里的 sidecar——`<pipeline-dir>/.sisyphus-ws.<job-dirname>.json`，
/// **不**放进 job 目录：job 目录是被 checkout 出来的用户仓库根，标记若在
/// 其内会（a）被 `checkout scm` 的 `clean -fd` 删掉、（b）污染用户 `git status`。
/// 放在 pipeline 目录（不进 checkout 范围）则两难俱免。
const MARKER_JOB_PREFIX: &str = ".sisyphus-ws.";
/// pipeline 目录标识标记文件名（记录原始 pipeline 名，用于 pipeline 名清洗冲突）。
/// 在 pipeline 目录里——pipeline 目录本身不被 checkout，不进任何仓库。
const MARKER_PIPELINE: &str = ".sisyphus-ws-pipeline.json";
/// 标记文件名前缀（采样器与列表跳过这些 sisyphus 元数据文件，不计入占用/不视为 job）。
const MARKER_PREFIX: &str = ".sisyphus-ws";
/// 清洗后目录名段字节上限（为 `-<N>` id 后缀留位，255 上限下取 244）。
const MAX_NAME_LEN: usize = 244;
/// 清洗冲突后缀尝试上限（防 runaway；实际冲突极罕见）。
const MAX_COLLISION_ATTEMPTS: u32 = 65_535;

// ============================================================
// 名称清洗
// ============================================================

/// 清洗目录名段：非 `[A-Za-z0-9._-]` 一律替换 `_`，空/`.`/`..` 归一为 `_`，
/// 超长按字节截断（替换后恒为 ASCII，字节截断安全）。允许 `.` 但把裸 `.`
/// 与 `..`（路径穿越）归一为 `_`。
fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        s = "_".to_string();
    }
    if s.len() > MAX_NAME_LEN {
        s.truncate(MAX_NAME_LEN);
    }
    s
}

// ============================================================
// 标记（目录身份元数据，随目录同生同灭）
// ============================================================

/// job 目录标记：原始 pipeline/job 名 + 最近使用时间（列表/清理还原真名）。
#[derive(Serialize, Deserialize)]
struct JobMarker {
    pipeline: String,
    job: String,
    last_used_at_ms: i64,
}

/// pipeline 目录标记：原始 pipeline 名（pipeline 名清洗冲突时还原真名）。
#[derive(Serialize, Deserialize)]
struct PipelineMarker {
    pipeline: String,
}

/// job 标记 sidecar 路径：`<pipeline-dir>/.sisyphus-ws.<job-dirname>.json`（与
/// job 目录同级、在 pipeline 目录里——不进 checkout 范围）。
fn job_marker_path(job_dir: &Path) -> Option<PathBuf> {
    let name = job_dir.file_name()?.to_str()?;
    job_dir
        .parent()
        .map(|p| p.join(format!("{MARKER_JOB_PREFIX}{name}.json")))
}

/// 读 job 标记 sidecar（不存在/损坏 = `None`，列表回退到目录名）。
fn read_job_marker(job_dir: &Path) -> Option<JobMarker> {
    read_json::<JobMarker>(&job_marker_path(job_dir)?)
}

/// 读 pipeline 目录标记。
fn read_pipeline_marker(dir: &Path) -> Option<PipelineMarker> {
    read_json::<PipelineMarker>(&dir.join(MARKER_PIPELINE))
}

/// 原子写 job 标记 sidecar（tmp + rename，防半截标记被读到——半截会回退到
/// 目录名，但原子写更干净）。失败记警告不判败（标记缺失有回退兜底）。
fn write_job_marker(job_dir: &Path, pipeline: &str, job: &str, last_used_at_ms: i64) {
    let Some(path) = job_marker_path(job_dir) else {
        return;
    };
    let marker = JobMarker {
        pipeline: pipeline.to_string(),
        job: job.to_string(),
        last_used_at_ms,
    };
    write_json_atomic(&path, &marker);
}

fn write_pipeline_marker(dir: &Path, pipeline: &str) {
    let marker = PipelineMarker {
        pipeline: pipeline.to_string(),
    };
    write_json_atomic(&dir.join(MARKER_PIPELINE), &marker);
}

/// 读 JSON 文件并反解（不存在/解析失败 = `None`）。
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 原子写 JSON（同目录 tmp → rename；rename 失败清理 tmp）。
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) {
    let text = match serde_json::to_string(value) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "标记序列化失败");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Unix 毫秒时间戳（尽力而为：系统时钟异常回退 0）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ============================================================
// 目录探测（resolve / find 共用）
// ============================================================

/// 探测结果：`Found` = 命中标记身份可复用；`Free` = 目录不存在可创建；
/// `Taken` = 存在但身份不符（清洗冲突）或无标记（遗留/手工目录）→ 试下一后缀。
enum Probe {
    Found(PathBuf),
    Free(PathBuf),
    Taken,
}

/// 按 `read_match` 谓词探测 `parent/<base>[-<N>]`：`read_match(candidate)`
/// 返回 `Some(true)` 命中、`Some(false)` 身份不符、`None` 无标记或目录不存在
/// 的 fallback 语义由调用方在闭包内裁决（无标记目录视为 `Some(false)` 以
/// 触发后缀回退，避免复用身份不明的遗留目录）。
fn probe<F>(parent: &Path, base: &str, mut read_match: F) -> Probe
where
    F: FnMut(&Path) -> Option<bool>,
{
    for idx in 0..=MAX_COLLISION_ATTEMPTS {
        // idx=0 取 base；冲突从 `-2` 起（首个副本为 `-2`，与「name / name-2 / name-3」
        // 习惯对齐，base 隐式为 1）。
        let name = if idx == 0 {
            base.to_string()
        } else {
            format!("{base}-{}", idx + 1)
        };
        let candidate = parent.join(&name);
        if !candidate.exists() {
            return Probe::Free(candidate);
        }
        if let Some(true) = read_match(&candidate) {
            return Probe::Found(candidate);
        }
        // 身份不符或无标记：试下一后缀（continue 隐式）。
    }
    // 超过上限：退回 Taken（不应发生）。
    Probe::Taken
}

// ============================================================
// 共享状态（组合根 + run_connection + Handle 共用，Clone）
// ============================================================

/// 工作区共享状态：根 + 活体上行发送器 + 占用采样源。`Clone`——组合根、
/// `run_connection`（set_live/读采样）、workspace Handle 各持一份共享同一内部。
///
/// 运行中 job 集合（残留检查）是独立机制 [`RunningJobs`]，不挂在这里——
/// 它是 runner（#59）的运行时状态，与工作区布局无关；本模块只提供机制，
/// dispatch 缝的 claim/release 归 #59。
#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    live: Arc<RwLock<Option<mpsc::Sender<ChannelMessage>>>>,
    usage: Option<Arc<dyn WorkspaceUsage>>,
}

impl Workspace {
    /// 以工作区根构造（无上行、无采样源）。采样源由 [`Workspace::with_usage`]
    /// 注入（组合根装配时挂低频采样器）。
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            live: Arc::new(RwLock::new(None)),
            usage: None,
        }
    }

    /// 挂载占用采样源（组合根把 [`WorkspaceSampler`] 注入；返回 self 便于链式）。
    pub fn with_usage(mut self, usage: Arc<dyn WorkspaceUsage>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// 最近一次采样的工作区占用字节数（无采样源 = 0）。心跳 [`DiskUsage`] 取此。
    pub fn workspace_bytes(&self) -> i64 {
        self.usage
            .as_ref()
            .map(|u| u.workspace_bytes())
            .unwrap_or(0)
    }

    /// 工作区根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析（必要时创建）job 工作区目录：`<root>/<pipeline>/<job>/`。pipeline
    /// 名与 job 名各自清洗，冲突按标记身份匹配追加 `-<N>` 后缀；新建/复用均
    /// 写/刷新 job 标记的 `last_used_at_ms`。返回绝对路径。
    pub fn resolve(&self, pipeline: &str, job: &str) -> io::Result<PathBuf> {
        // pipeline 目录：按 pipeline 标记身份匹配。
        let p_dir = match probe(&self.root, &sanitize(pipeline), |d| {
            match read_pipeline_marker(d) {
                Some(m) => Some(m.pipeline == pipeline),
                None => Some(false), // 遗留/无标记目录：不复用，触发后缀回退
            }
        }) {
            Probe::Found(p) => p,
            Probe::Free(p) => {
                std::fs::create_dir_all(&p)?;
                write_pipeline_marker(&p, pipeline);
                p
            }
            Probe::Taken => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "pipeline 目录清洗冲突耗尽后缀空间",
                ))
            }
        };
        // job 目录：按 job 标记（pipeline + job 双匹配）。
        let j_dir = match probe(&p_dir, &sanitize(job), |d| match read_job_marker(d) {
            Some(m) => Some(m.pipeline == pipeline && m.job == job),
            None => Some(false),
        }) {
            Probe::Found(p) => {
                // 复用：刷新最近使用时间。
                write_job_marker(&p, pipeline, job, now_ms());
                p
            }
            Probe::Free(p) => {
                std::fs::create_dir_all(&p)?;
                write_job_marker(&p, pipeline, job, now_ms());
                p
            }
            Probe::Taken => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "job 目录清洗冲突耗尽后缀空间",
                ))
            }
        };
        Ok(j_dir)
    }

    /// 查找（不创建）已存在的 job 工作区目录。用于清理指令定位——清理不应
    /// 创建目录。不存在返回 `None`。pipeline 定位复用 [`Self::find_pipeline_dir`。
    pub fn find(&self, pipeline: &str, job: &str) -> Option<PathBuf> {
        let p_dir = self.find_pipeline_dir(pipeline)?;
        match probe(&p_dir, &sanitize(job), |d| match read_job_marker(d) {
            Some(m) => Some(m.pipeline == pipeline && m.job == job),
            None => Some(false),
        }) {
            Probe::Found(p) => Some(p),
            _ => None,
        }
    }

    /// 列出全部工作区：遍历根两层（pipeline 目录 → job 目录），按标记还原
    /// (pipeline, job, path, last_used)；标记缺失回退到目录名 + 目录 mtime。
    pub fn list(&self) -> Vec<WorkspaceEntry> {
        let mut out = Vec::new();
        let Ok(p_entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for p_entry in p_entries.flatten() {
            let p_dir = p_entry.path();
            if !p_dir.is_dir() {
                continue;
            }
            let pipeline = read_pipeline_marker(&p_dir)
                .map(|m| m.pipeline)
                .or_else(|| Some(p_entry.file_name().to_string_lossy().into_owned()))
                .unwrap_or_default();
            let Ok(j_entries) = std::fs::read_dir(&p_dir) else {
                continue;
            };
            for j_entry in j_entries.flatten() {
                let j_dir = j_entry.path();
                if !j_dir.is_dir() {
                    continue;
                }
                let marker = read_job_marker(&j_dir);
                let (job, last_used) = match marker {
                    Some(m) => (m.job, m.last_used_at_ms),
                    None => (dir_name(&j_entry), mtime_ms(&j_dir).unwrap_or(0)),
                };
                out.push(WorkspaceEntry {
                    pipeline: pipeline.clone(),
                    job,
                    path: j_dir.to_string_lossy().into_owned(),
                    last_used_at_ms: last_used,
                });
            }
        }
        out
    }

    /// 清理工作区：单 job / 单 pipeline / 全清。删树严格限定在 `self.root`
    /// 之下——`remove_dir_all` 仅作用于经标记/根定位的子目录，缓存根
    /// （`<data>/cache/`，工作区根的兄弟独立目录）永不被触及。返回删除的
    /// 顶层目录数（用于日志/可观测，不回传 Server——清理无 ack 帧）。
    ///
    /// - `pipeline` 空 + `job` 空 → 全清（删根下每个 pipeline 目录，保根）。
    /// - `pipeline` 有 + `job` 空 → 清该 pipeline（删其目录树）。
    /// - `pipeline` 有 + `job` 有 → 清该单 job（删其目录）。
    /// - `pipeline` 空 + `job` 有 → 范围未定义，no-op（记警告）。
    pub fn clean(&self, pipeline: &str, job: &str) -> io::Result<usize> {
        if pipeline.is_empty() && job.is_empty() {
            // 全清：删根下每个子目录（pipeline 目录），保根本身与根级标记。
            let mut removed = 0;
            for entry in std::fs::read_dir(&self.root)?.flatten() {
                let path = entry.path();
                if path.is_dir() && is_under(&path, &self.root) {
                    remove_dir_all_best_effort(&path);
                    removed += 1;
                }
            }
            return Ok(removed);
        }
        if !pipeline.is_empty() && job.is_empty() {
            // 单 pipeline：按标记定位其目录（清洗冲突时取真名匹配的那个）。
            if let Some(p_dir) = self.find_pipeline_dir(pipeline) {
                remove_dir_all_best_effort(&p_dir);
                return Ok(1);
            }
            return Ok(0);
        }
        if !pipeline.is_empty() && !job.is_empty() {
            // 单 job：按标记定位其目录（不创建）。删目录 + 删其在 pipeline 目录
            // 里的 sidecar 标记（标记不进 job 目录，故删 job 目录不会顺带删标记）。
            if let Some(j_dir) = self.find(pipeline, job) {
                remove_dir_all_best_effort(&j_dir);
                remove_job_marker_best_effort(&j_dir);
                return Ok(1);
            }
            return Ok(0);
        }
        // pipeline 空 + job 有：范围未定义，no-op。
        tracing::warn!(job = %job, "清理指令缺 pipeline（范围未定义），no-op");
        Ok(0)
    }

    /// 按标记定位 pipeline 目录（清洗冲突时取真名匹配的 `-<N>` 目录）。无标记
    /// 目录返回 `Some(false)`（不复用，触发后缀回退）——与 [`probe`] 约定一致。
    fn find_pipeline_dir(&self, pipeline: &str) -> Option<PathBuf> {
        match probe(&self.root, &sanitize(pipeline), |d| match read_pipeline_marker(d) {
            Some(m) => Some(m.pipeline == pipeline),
            None => Some(false),
        }) {
            Probe::Found(p) => Some(p),
            _ => None,
        }
    }

    /// 注入/清除活体上行发送器（连接期 `run_connection` 调用；断线置 `None`，
    /// 列表响应仅落日志不外送）。与 logbuf `set_live` 同款。
    pub async fn set_live(&self, tx: Option<mpsc::Sender<ChannelMessage>>) {
        *self.live.write().await = tx;
    }

    /// 处理一帧工作区指令：列表 → 构造 `WorkspaceList` 经活体上行送出；清理
    /// → 执行删树（无 ack 帧）。无活体连接时列表帧仅记日志（断线不丢指令，
    /// 重连不重发——列表是查询非状态，UI 重发即可）。
    pub async fn handle(&self, cmd: WorkspaceCommand) {
        match cmd.kind {
            Some(WorkspaceKind::List(_)) => {
                let entries = self.list();
                let msg = ChannelMessage {
                    kind: Some(Kind::WorkspaceList(WorkspaceList { entries })),
                };
                self.send_up(msg).await;
            }
            Some(WorkspaceKind::Clean(req)) => {
                let (pipeline, job) = (req.pipeline, req.job);
                match self.clean(&pipeline, &job) {
                    Ok(n) => tracing::info!(pipeline = %pipeline, job = %job, removed = n, "工作区清理完成"),
                    Err(e) => tracing::warn!(error = %e, "工作区清理失败"),
                }
            }
            None => tracing::warn!("工作区指令缺 kind，忽略"),
        }
    }

    /// 经活体发送器上行一帧（断线 = 无发送器，记日志；单 writer 保写序——
    /// `out_tx` 与心跳/日志同一通道）。
    async fn send_up(&self, msg: ChannelMessage) {
        let live = self.live.read().await;
        if let Some(tx) = live.as_ref() {
            if tx.send(msg).await.is_err() {
                tracing::warn!("工作区列表响应发送失败：通道已关闭");
            }
        } else {
            tracing::warn!("无活体连接，工作区列表响应未外送（断线；UI 可重发查询）");
        }
    }
}

/// 目录条目文件名（损失 String；非法 UTF-8 回退 lossy）。
fn dir_name(entry: &DirEntry) -> String {
    entry.file_name().to_string_lossy().into_owned()
}

/// 目录 mtime → Unix 毫秒。
fn mtime_ms(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .ok()
}

/// `path` 是否严格位于 `base` 之下（防越界删到根外——清洗已禁分隔符，此为
/// 二重保险：`remove_dir_all` 的目标必须以 `base` 为前缀且非 `base` 本身）。
fn is_under(path: &Path, base: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(base) = base.canonicalize() else {
        return false;
    };
    path != base && path.starts_with(&base)
}

/// 删树（尽力而为：不存在忽略；失败记警告——清理是维护动作，删不掉比误删好）。
fn remove_dir_all_best_effort(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "工作区目录已删除"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "工作区目录删除失败"),
    }
}

/// 删 job 标记 sidecar（单 job 清理后——标记在 pipeline 目录里，删 job 目录
/// 不会顺带删它）。尽力而为：不存在/删失败忽略（孤儿标记无害，下次 list 回退到目录名）。
fn remove_job_marker_best_effort(job_dir: &Path) {
    if let Some(marker) = job_marker_path(job_dir) {
        match std::fs::remove_file(&marker) {
            Ok(()) => tracing::debug!(path = %marker.display(), "job 标记 sidecar 已删除"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %marker.display(), error = %e, "job 标记 sidecar 删除失败"),
        }
    }
}

// ============================================================
// 运行中 job 集合（残留检查，ADR-0011：不做锁文件）
// ============================================================

/// 内存级「运行中 job 集合」句柄（`Clone` 共享）。重跑下发同 job 已在跑 →
/// [`RunningJobs::claim`] 返回 `false` → 拒收。runner（#59）在收 JobSpec 前
/// claim、终态 release。`Arc<Mutex<HashSet>>` 共享自 [`Workspace::running`]。
#[derive(Clone, Default)]
pub struct RunningJobs {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl RunningJobs {
    /// 从共享集合构造（与 [`Workspace::running`] 同源，#59 统一在途上报）。
    pub fn from_shared(inner: Arc<Mutex<HashSet<String>>>) -> Self {
        Self { inner }
    }

    /// 占用 job：未在跑则记入并返回 `true`（接受）；已在跑返回 `false`（拒收）。
    pub fn claim(&self, job_id: &str) -> bool {
        let mut g = self.inner.lock().expect("运行集合锁");
        if g.contains(job_id) {
            return false;
        }
        g.insert(job_id.to_string());
        true
    }

    /// 释放 job（终态后调用；不在集合内无副作用）。
    pub fn release(&self, job_id: &str) {
        self.inner.lock().expect("运行集合锁").remove(job_id);
    }

    /// 是否在跑（测试/可观测）。
    pub fn contains(&self, job_id: &str) -> bool {
        self.inner.lock().expect("运行集合锁").contains(job_id)
    }

    /// 快照（重连在途上报 #59 用；当前由 Agent 根 `in_flight` 承载，此为
    /// #59 统一时的迁移面）。
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().expect("运行集合锁").clone().into_iter().collect()
    }
}

// ============================================================
// 工作区占用采样（低频后台遍历，喂心跳 DiskUsage）
// ============================================================

/// 工作区占用读取面（心跳采样源）。[`WorkspaceSampler`] 实现；测试可注入
/// 假实现绕过真实遍历。
pub trait WorkspaceUsage: Send + Sync {
    /// 最近一次采样的工作区占用字节数。
    fn workspace_bytes(&self) -> i64;
}

/// 后台低频工作区占用采样器：spawn 一个循环，按 `interval` 遍历根求和写入
/// 原子变量；spawn 时先采样一次（心跳首帧即有值）。`Arc` 共享——组合根持
/// 一份喂心跳，`spawn` 持一份驱动循环。
pub struct WorkspaceSampler {
    root: PathBuf,
    bytes: AtomicI64,
}

impl WorkspaceSampler {
    /// 以工作区根构造（初始 0，spawn 后立即采样）。
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            bytes: AtomicI64::new(0),
        }
    }

    /// 立即采样一次（同步遍历；测试可直接调用绕过 spawn 计时）。
    pub fn sample_once(&self) {
        self.bytes.store(walk_size(&self.root), Ordering::Relaxed);
    }

    /// 后台采样循环：先立即采样一次，再按 `interval` 周期采样。返回句柄供
    /// `run` 在退出时 abort。**须在 tokio runtime 上下文调用**（spawn）。
    pub fn spawn(self: Arc<Self>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.sample_once();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                self.sample_once();
            }
        })
    }
}

impl WorkspaceUsage for WorkspaceSampler {
    fn workspace_bytes(&self) -> i64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// 递归求和根下所有常规文件字节（尽力而为：目录读失败/文件元数据失败跳过；
/// 跳过 `.sisyphus-ws*` 标记文件——sisyphus 元数据不计入占用）。
fn walk_size(root: &Path) -> i64 {
    let mut total: i64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(MARKER_PREFIX))
                {
                    continue;
                }
                total = total.saturating_add(meta.len() as i64);
            }
        }
    }
    total
}

// ============================================================
// ${SISY_WORKSPACE} 占位替换（runner #59 执行前用）
// ============================================================

/// 把 `${SISY_WORKSPACE}` 替换为 `workspace` 绝对路径；`$${SISY_WORKSPACE}`
/// 转义为字面量 `${SISY_WORKSPACE}`（与 Server 端 `${}` 转义纪律一致）。其余
/// 文本原样。ADR-0006/0011：仅此一个内置变量 Agent 端解析，when 表达式禁用
/// 仍在 Server 端校验。
pub fn expand_sisy_workspace(text: &str, workspace: &Path) -> String {
    let path = workspace.to_string_lossy();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if text[i..].starts_with(PLACEHOLDER_ESCAPED) {
            out.push_str(PLACEHOLDER);
            i += PLACEHOLDER_ESCAPED.len();
        } else if text[i..].starts_with(PLACEHOLDER) {
            out.push_str(&path);
            i += PLACEHOLDER.len();
        } else {
            // ASCII 安全推一个字符（UTF-8 边界：按 char 推进）。
            let ch = text[i..].chars().next().expect("非空尾段");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

// ============================================================
// 句柄（下行分派循环 owner）
// ============================================================

/// workspace 句柄：持有下行接收端、共享状态与收帧观测。`run` 消费 self，
/// 共享状态经 [`Workspace`]（`Clone`）另由组合根 / `run_connection` 持有。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    state: Workspace,
    receipts: ReceiptLog,
}

impl Handle {
    /// 以分派接收端、共享状态与收帧观测构造。
    pub fn new(rx: mpsc::Receiver<ChannelMessage>, state: Workspace, receipts: ReceiptLog) -> Self {
        Self {
            rx,
            state,
            receipts,
        }
    }

    /// 共享状态（组合根装配后供 `run_connection` set_live / 测试取用）。
    pub fn state(&self) -> &Workspace {
        &self.state
    }

    /// 下行循环：收 `WorkspaceCommand` 即交共享状态处理（列表上行/清理删树）。
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            let kind_label = match msg.kind {
                Some(Kind::WorkspaceCmd(_)) => "workspace",
                _ => "other",
            };
            self.receipts
                .lock()
                .expect("观测锁")
                .push(kind_label.to_string());
            match msg.kind {
                Some(Kind::WorkspaceCmd(cmd)) => self.state.handle(cmd).await,
                // 冗余握手/未知变体：分派面已过滤，此处兜底忽略。
                _ => tracing::warn!(?msg, "workspace 收到非工作区指令，忽略"),
            }
        }
    }
}

// ============================================================
// 单元测试（纯逻辑 + 真实 FS；TDD 红→绿）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sisyphus_proto::agent::WorkspaceCleanRequest;
    use sisyphus_proto::agent::workspace_command::Kind as WorkspaceKind;

    /// 临时工作区根 + Workspace。
    fn ws() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("临时工作区根");
        let ws = Workspace::new(dir.path().to_path_buf());
        (dir, ws)
    }

    // --- 名称清洗 ---

    #[test]
    fn sanitize_replaces_disallowed_chars_and_preserves_allowed() {
        assert_eq!(sanitize("hello"), "hello");
        assert_eq!(sanitize("a-b_c.1"), "a-b_c.1");
        // 空格、斜杠、冒号、竖线、中文 → 下划线。
        assert_eq!(sanitize("my job"), "my_job");
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("a:b"), "a_b");
        assert_eq!(sanitize("a|b"), "a_b");
        assert_eq!(sanitize("构建"), "__", "两个非 ASCII 字符各替换为 _");
    }

    #[test]
    fn sanitize_neutralizes_dot_dotdot_and_empty() {
        // 路径穿越防护：裸 `.` 与 `..` 归一为 `_`（`.` 字符本身在合法集内，
        // 但裸点/双点是路径穿越，必须归一）。
        assert_eq!(sanitize("."), "_");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize(""), "_");
        // 带后缀的点目录名保留（非穿越）。
        assert_eq!(sanitize(".hidden"), ".hidden");
        assert_eq!(sanitize("foo.bar"), "foo.bar");
    }

    #[test]
    fn sanitize_truncates_overlong() {
        let long = "a".repeat(300);
        let s = sanitize(&long);
        assert_eq!(s.len(), MAX_NAME_LEN);
        assert!(s.chars().all(|c| c == 'a'));
    }

    // --- resolve / 标记 / 冲突 ---

    #[tokio::test]
    async fn resolve_creates_dir_and_marker() {
        let (_dir, ws) = ws();
        let p = ws.resolve("pipe", "job").unwrap();
        assert!(p.is_dir(), "resolve 创建目录");
        assert_eq!(p, ws.root().join("pipe").join("job"));
        let marker = read_job_marker(&p).expect("标记已写");
        assert_eq!(marker.pipeline, "pipe");
        assert_eq!(marker.job, "job");
        assert!(marker.last_used_at_ms > 0, "最近使用时间已记");
    }

    #[tokio::test]
    async fn resolve_writes_marker_sidecar_outside_job_dir() {
        // 标记是 pipeline 目录里的 sidecar，不进 job 目录——job 目录是被
        // checkout 出来的用户仓库根，标记在其内会被 `clean -fd` 删掉、
        // 污染 `git status`。侧车放在 pipeline 目录两难俱免。
        let (_dir, ws) = ws();
        let job_dir = ws.resolve("pipe", "job").unwrap();
        // job 目录内无任何 sisyphus 标记文件（用户仓库根干净）。
        for entry in std::fs::read_dir(&job_dir).unwrap().flatten() {
            let name = entry.file_name();
            assert!(
                !name.to_string_lossy().starts_with(MARKER_PREFIX),
                "job 目录内不应有 sisyphus 标记文件：{:?}",
                name
            );
        }
        // 标记 sidecar 在 pipeline 目录里，命名为 .sisyphus-ws.<job-dirname>.json。
        let sidecar = job_marker_path(&job_dir).expect("sidecar 路径");
        assert!(sidecar.exists(), "sidecar 标记存在于 pipeline 目录");
        assert_eq!(
            sidecar,
            job_dir.parent().unwrap().join(".sisyphus-ws.job.json"),
            "sidecar 命名 = 前缀 + job 目录名 + .json"
        );
        assert!(sidecar.parent() == job_dir.parent(), "sidecar 与 job 目录同级");
    }

    #[tokio::test]
    async fn resolve_reuses_same_job_and_refreshes_last_used() {
        let (_dir, ws) = ws();
        let p1 = ws.resolve("pipe", "job").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let p2 = ws.resolve("pipe", "job").unwrap();
        assert_eq!(p1, p2, "同 (pipeline, job) 复用同一目录");
        // last_used 刷新（>= p1 时刻）。
        let m2 = read_job_marker(&p2).unwrap();
        assert!(m2.last_used_at_ms > 0);
    }

    #[tokio::test]
    async fn resolve_distinct_jobs_get_distinct_dirs() {
        let (_dir, ws) = ws();
        let a = ws.resolve("pipe", "job-a").unwrap();
        let b = ws.resolve("pipe", "job-b").unwrap();
        assert_ne!(a, b);
        assert_eq!(a, ws.root().join("pipe").join("job-a"));
        assert_eq!(b, ws.root().join("pipe").join("job-b"));
    }

    #[tokio::test]
    async fn resolve_job_name_collision_appends_suffix() {
        // 两个不同 job 名清洗到同一段 → 第二个追加 -2 后缀。
        // "job 1"（空格）与 "job_1"（字面）都清洗到 "job_1"。
        let (_dir, ws) = ws();
        let a = ws.resolve("pipe", "job 1").unwrap();
        let b = ws.resolve("pipe", "job_1").unwrap();
        assert_eq!(a, ws.root().join("pipe").join("job_1"));
        assert_eq!(b, ws.root().join("pipe").join("job_1-2"), "冲突追加 -2");
        // 各自身份由标记还原。
        assert_eq!(read_job_marker(&a).unwrap().job, "job 1");
        assert_eq!(read_job_marker(&b).unwrap().job, "job_1");
    }

    #[tokio::test]
    async fn resolve_pipeline_name_collision_appends_suffix() {
        // 两个不同 pipeline 名清洗到同一段 → 第二个的 pipeline 目录追加 -2。
        // "my pipe"（空格）与 "my|pipe"（竖线）都清洗到 "my_pipe"。
        let (_dir, ws) = ws();
        let a = ws.resolve("my pipe", "job").unwrap();
        let b = ws.resolve("my|pipe", "job").unwrap();
        assert_eq!(a, ws.root().join("my_pipe").join("job"));
        assert_eq!(
            b,
            ws.root().join("my_pipe-2").join("job"),
            "pipeline 冲突追加 -2"
        );
        assert_eq!(read_pipeline_marker(a.parent().unwrap()).unwrap().pipeline, "my pipe");
        assert_eq!(read_pipeline_marker(b.parent().unwrap()).unwrap().pipeline, "my|pipe");
    }

    #[tokio::test]
    async fn find_does_not_create_and_returns_existing() {
        let (_dir, ws) = ws();
        assert!(ws.find("pipe", "job").is_none(), "不存在返回 None");
        let created = ws.resolve("pipe", "job").unwrap();
        let found = ws.find("pipe", "job").expect("resolve 后可 find");
        assert_eq!(found, created);
        // find 不创建新目录。
        assert!(ws.find("pipe", "other").is_none());
    }

    // --- list ---

    #[tokio::test]
    async fn list_returns_entries_with_true_names_and_paths() {
        let (_dir, ws) = ws();
        ws.resolve("pipe-a", "job 1").unwrap();
        ws.resolve("pipe-a", "job_1").unwrap(); // 冲突 → job_1-2，列表还原真名
        ws.resolve("pipe-b", "job-x").unwrap();
        let mut entries = ws.list();
        entries.sort_by_key(|e| (e.pipeline.clone(), e.job.clone()));
        let names: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.pipeline.clone(), e.job.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("pipe-a".into(), "job 1".into()),
                ("pipe-a".into(), "job_1".into()),
                ("pipe-b".into(), "job-x".into()),
            ],
            "列表还原真名（含冲突后缀的目录还原原始 job 名）"
        );
        for e in &entries {
            assert!(e.last_used_at_ms > 0, "last_used 取自标记");
            assert!(Path::new(&e.path).is_dir(), "path 指向真实目录");
        }
    }

    #[tokio::test]
    async fn list_falls_back_to_dir_name_for_unmarked_dirs() {
        let (_dir, ws) = ws();
        // 手工建一个无标记目录（遗留/外部）。
        std::fs::create_dir_all(ws.root().join("legacy-pipe").join("legacy-job")).unwrap();
        let entries = ws.list();
        let e = entries
            .iter()
            .find(|e| e.job == "legacy-job")
            .expect("回退到目录名");
        assert_eq!(e.pipeline, "legacy-pipe");
        assert!(e.last_used_at_ms >= 0, "mtime 回退");
    }

    // --- clean ---

    #[tokio::test]
    async fn clean_single_job_removes_only_that_dir() {
        let (_dir, ws) = ws();
        let a = ws.resolve("pipe", "job-a").unwrap();
        let b = ws.resolve("pipe", "job-b").unwrap();
        std::fs::write(a.join("out.txt"), b"x").unwrap();
        let a_sidecar = job_marker_path(&a).expect("a sidecar");
        assert!(a_sidecar.exists(), "清理前 sidecar 存在");
        let n = ws.clean("pipe", "job-a").unwrap();
        assert_eq!(n, 1);
        assert!(!a.exists(), "job-a 已删");
        assert!(!a_sidecar.exists(), "job-a 的 sidecar 标记随清理删除");
        assert!(b.exists(), "job-b 保留");
        assert!(ws.find("pipe", "job-b").is_some());
    }

    #[tokio::test]
    async fn clean_pipeline_removes_whole_pipeline() {
        let (_dir, ws) = ws();
        let a = ws.resolve("pipe-a", "job-1").unwrap();
        let b = ws.resolve("pipe-b", "job-1").unwrap();
        let n = ws.clean("pipe-a", "").unwrap();
        assert_eq!(n, 1);
        assert!(!a.exists(), "pipe-a 整树删除");
        assert!(b.exists(), "pipe-b 保留");
    }

    #[tokio::test]
    async fn clean_all_removes_every_pipeline_preserving_root() {
        let (_dir, ws) = ws();
        let a = ws.resolve("pipe-a", "job").unwrap();
        let b = ws.resolve("pipe-b", "job").unwrap();
        let n = ws.clean("", "").unwrap();
        assert_eq!(n, 2, "删了两个 pipeline 目录");
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(ws.root().is_dir(), "工作区根本身保留");
    }

    #[tokio::test]
    async fn clean_missing_is_noop() {
        let (_dir, ws) = ws();
        assert_eq!(ws.clean("nope", "nope").unwrap(), 0, "不存在的 job → 0");
        assert_eq!(ws.clean("nope", "").unwrap(), 0, "不存在的 pipeline → 0");
    }

    #[tokio::test]
    async fn clean_never_touches_cache_sibling() {
        // 缓存根 = 工作区根的兄弟目录（<data>/cache）。清理只作用于工作区根
        // 之下，缓存根永不被触及。
        let data = tempfile::tempdir().expect("临时数据目录");
        let ws_root = data.path().join("workspaces");
        let cache_root = data.path().join("cache");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::create_dir_all(cache_root.join("some-pipe").join("some-key")).unwrap();
        std::fs::write(cache_root.join("some-pipe").join("some-key").join("artifact"), b"v").unwrap();
        let ws = Workspace::new(ws_root);
        ws.resolve("pipe", "job").unwrap();
        ws.clean("", "").unwrap();
        assert!(
            cache_root.join("some-pipe").join("some-key").join("artifact").exists(),
            "缓存目录未被清理触及"
        );
    }

    // --- 运行中 job 集合（残留检查）---

    #[test]
    fn running_jobs_claim_accepts_first_rejects_duplicate() {
        let set = Arc::new(Mutex::new(HashSet::new()));
        let rj = RunningJobs::from_shared(set);
        assert!(rj.claim("job-1"), "首次占用接受");
        assert!(!rj.claim("job-1"), "同 job 已在跑 → 拒收");
        assert!(rj.contains("job-1"));
        assert!(rj.claim("job-2"), "不同 job 各自占用");
        rj.release("job-1");
        assert!(!rj.contains("job-1"), "释放后不再在跑");
        assert!(rj.claim("job-1"), "释放后可再次占用");
    }

    #[test]
    fn running_jobs_release_absent_is_noop() {
        let rj = RunningJobs::default();
        rj.release("never"); // 不 panic
        assert_eq!(rj.snapshot().len(), 0);
    }

    // --- ${SISY_WORKSPACE} 展开 ---

    #[test]
    fn expand_substitutes_placeholder_and_escape() {
        let ws = Path::new("/srv/ws/pipe/job");
        assert_eq!(
            expand_sisy_workspace("${SISY_WORKSPACE}/src", ws),
            "/srv/ws/pipe/job/src"
        );
        assert_eq!(
            expand_sisy_workspace("$${SISY_WORKSPACE}", ws),
            "${SISY_WORKSPACE}",
            "转义输出字面量"
        );
        assert_eq!(
            expand_sisy_workspace("no var here", ws),
            "no var here"
        );
        assert_eq!(
            expand_sisy_workspace("${SISY_WORKSPACE}${SISY_WORKSPACE}", ws),
            "/srv/ws/pipe/job/srv/ws/pipe/job",
            "多次替换"
        );
        assert_eq!(
            expand_sisy_workspace("$${SISY_WORKSPACE}/x ${SISY_WORKSPACE}/y", ws),
            "${SISY_WORKSPACE}/x /srv/ws/pipe/job/y",
            "转义与替换混合"
        );
    }

    // --- 采样器 ---

    #[tokio::test]
    async fn sampler_counts_files_skipping_markers() {
        let dir = tempfile::tempdir().expect("临时工作区根");
        let root = dir.path().to_path_buf();
        // 模拟 resolve 写入的新布局：pipeline 目录里放 pipeline 标记与 job
        // sidecar 标记（不进 job 目录），job 目录里只放产出文件。
        let pipe_dir = root.join("pipe");
        let job_dir = pipe_dir.join("job");
        std::fs::create_dir_all(&job_dir).unwrap();
        std::fs::write(pipe_dir.join(MARKER_PIPELINE), b"{}").unwrap();
        std::fs::write(
            pipe_dir.join(format!("{MARKER_JOB_PREFIX}job.json")),
            b"{}",
        )
        .unwrap();
        std::fs::write(job_dir.join("out.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(job_dir.join("log.txt"), b"hello").unwrap();
        let sampler = WorkspaceSampler::new(root);
        sampler.sample_once();
        // 1000 + 5 = 1005；标记文件（pipeline 目录里的 sidecar + pipeline 标记）
        // 以 `.sisyphus-ws` 前缀跳过，不计入占用。
        assert_eq!(sampler.workspace_bytes(), 1005);
        assert!(WorkspaceUsage::workspace_bytes(&sampler) == 1005);
    }

    #[tokio::test]
    async fn sampler_spawn_samples_immediately_and_periodically() {
        let dir = tempfile::tempdir().expect("临时工作区根");
        let sampler = Arc::new(WorkspaceSampler::new(dir.path().to_path_buf()));
        std::fs::write(dir.path().join("file"), b"abc").unwrap();
        // 注入短间隔（50ms）避免真实 10 分钟 sleep。
        let handle = sampler.clone().spawn(Duration::from_millis(50));
        // spawn 内先采样一次：轮询到值出现（不依赖 tokio 调度时序）。
        let mut seen = false;
        for _ in 0..50 {
            if sampler.workspace_bytes() == 3 {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(seen, "spawn 立即采样应可见 3 字节");
        // 周期采样保持值（文件未变）。
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(sampler.workspace_bytes(), 3, "周期采样保持值");
        handle.abort();
    }

    #[tokio::test]
    async fn sampler_empty_root_is_zero() {
        let dir = tempfile::tempdir().expect("临时空根");
        let sampler = WorkspaceSampler::new(dir.path().to_path_buf());
        sampler.sample_once();
        assert_eq!(sampler.workspace_bytes(), 0);
    }

    // --- handle 集成（指令 → 状态）---

    #[tokio::test]
    async fn handle_list_sends_workspace_list_uplink() {
        let (_dir, ws) = ws();
        ws.resolve("pipe", "job").unwrap();
        let (tx, mut rx) = mpsc::channel::<ChannelMessage>(8);
        ws.set_live(Some(tx)).await;

        ws.handle(WorkspaceCommand {
            kind: Some(WorkspaceKind::List(Default::default())),
        })
        .await;

        let msg = rx.recv().await.expect("上行列表响应");
        let list = match msg.kind {
            Some(Kind::WorkspaceList(l)) => l,
            _ => panic!("期望 WorkspaceList"),
        };
        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.entries[0].pipeline, "pipe");
        assert_eq!(list.entries[0].job, "job");
    }

    #[tokio::test]
    async fn handle_clean_executes_removal() {
        let (_dir, ws) = ws();
        let p = ws.resolve("pipe", "job").unwrap();
        std::fs::write(p.join("out"), b"x").unwrap();
        ws.handle(WorkspaceCommand {
            kind: Some(WorkspaceKind::Clean(WorkspaceCleanRequest {
                pipeline: "pipe".into(),
                job: "job".into(),
            })),
        })
        .await;
        assert!(!p.exists(), "handle 清理已删");
    }

    #[tokio::test]
    async fn handle_list_without_live_does_not_send() {
        let (_dir, ws) = ws();
        ws.resolve("pipe", "job").unwrap();
        // 无 set_live → send_up 仅记日志，不 panic、不阻塞。
        ws.handle(WorkspaceCommand {
            kind: Some(WorkspaceKind::List(Default::default())),
        })
        .await;
    }
}

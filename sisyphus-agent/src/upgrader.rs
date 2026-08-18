//! upgrader：升级指令 → 排空 → 下载 → sha256 校验 → 原子换入 → spawn 新进程
//! （ADR-0017；票 B3-T9 / #61）。
//!
//! Server 经 gRPC 通道下发 [`UpgradeCommand`]，Agent 自行下载、校验、替换二进制
//! 并重启。升级全流程任一步失败：**保持旧版本、上报错误、继续跑**（ADR-0017
//! 核心不变式）。机制：
//!
//! - **排空**（[`DrainGate`]）：收到升级指令即置 draining，通知 runner 停接新任务
//!   （新 JobSpec → ack accepted=false），等运行中任务全部终态后再换入。**不设
//!   超时**——管理员等不及可取消任务解锁（取消经 runner 常态 cancel 通路，终态
//!   即释放并唤醒排空）。升级期间 JobSpec 不再分派（runner 在 draining 下拒收）。
//! - **下载**（[`Downloader`] 缝）：reqwest GET `download_url`，Bearer agent token。
//!   下载到同目录临时文件（同卷 rename 原子）。失败 = 弃、保持旧版、上报、继续。
//! - **sha256 校验**：下载字节 sha256 与指令 `sha256` 不符 = 弃、保持旧版、上报、
//!   继续（不换入）。
//! - **原子换入**（[`swap_binary`]）：Unix rename 覆盖；Windows 先 rename 运行中
//!   exe 再写新文件。旧二进制保留为 `.old`（回退锚点）。换入内部带回退：rename
//!   旧→.old 成功但 新→当前 失败时把 .old 换回当前，不留空窗。
//! - **spawn 新进程**（[`Spawner`] 缝）：以继承的参数（`std::env::args` 去首）+
//!   环境（tokio Command 默认继承）spawn 当前路径（换入后的新二进制），甄别 grace
//!   窗内是否健康。健康 → 旧进程退出（经 exit watch 通知 `Agent::run`）。失败 →
//!   计入连续失败计数（持久化 `<data>/agent.json`），最多重试 3 次；连续 3 次仍
//!   失败 → [`rollback_binary`] 把 `.old` 换回当前、上报 [`UpgradePhase::UpgradeFallback`]
//!   并继续跑（旧版本重连上报失败原因）。
//!
//! **可测性**：下载与 spawn 各是一个 trait 缝（`Arc<dyn>` 注入）——生产用 real
//! （reqwest + tokio spawn 甄别），测试用 fake（canned 字节 / 记录路径 + 配定结果），
//! 不真下载、不真重启进程。当前二进制路径同样可注入（生产 `std::env::current_exe`，
//! 测试临时文件）。排空闸门 / 升级阶段上报也经小结构可直驱单测。
//!
//! **阶段上报**（[`UpgradeUplink`]）：排空/下载/换入/重启/退回各阶段经活体上行发送器
//! 随通道上报（与 runner / cache / workspace 同款 `set_live`）；断线时最新阶段存
//! `pending`，重连 `flush_pending` 补发（「重连上报失败原因」）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sisyphus_proto::agent::{
    ChannelMessage, UpgradeCommand, UpgradePhase, UpgradeStatus, channel_message::Kind,
};
use tokio::sync::{Notify, RwLock, mpsc, watch};

use crate::ReceiptLog;

/// 旧二进制保留后缀（`<bin>.old`，回退锚点）。
const OLD_SUFFIX: &str = ".old";
/// 下载临时文件后缀（`<bin>.dl-tmp`，同目录同卷，rename 原子）。
const DL_TMP_SUFFIX: &str = ".dl-tmp";
/// 连续启动失败上限：达到即自动退回 `.old`（ADR-0017）。
const MAX_START_FAILURES: u32 = 3;
/// spawn 甄别 grace 窗：新进程在该窗内退出 = 启动失败；存活过窗 = 健康。
/// 仅生产 [`ProcessSpawner`] 用；测试 fake 不经此窗。
const SPAWN_GRACE: Duration = Duration::from_secs(2);
/// 排空等待的兜底轮询间隔（Notify 丢通知兜底；正常路径 Notify 即时唤醒）。
const DRAIN_POLL: Duration = Duration::from_millis(500);

// ============================================================
// 排空闸门（runner 与 upgrader 共享）
// ============================================================

/// 排空闸门：runner 据 `draining` 拒接新任务；upgrader 置位并等运行中任务全部
/// 终态（[`DrainGate::wait_drained`]）。`Clone`——runner Handle 与 upgrader Handle
/// 各持一份共享同一内部（`Arc`）。释放通知经 [`Notify`]：runner 每次释放一个
/// job 即 [`DrainGate::notify_released`]，upgrader 的 `wait_drained` 据此唤醒重查
/// 在途集；Notify 丢通知由 `DRAIN_POLL` 兜底轮询兜住。
#[derive(Clone, Default)]
pub struct DrainGate {
    draining: Arc<AtomicBool>,
    released: Arc<Notify>,
}

impl DrainGate {
    /// 新建（非排空态）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前是否排空中（runner 收到新 JobSpec 即据此拒收）。
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// 置/清排空态（upgrader 在升级开始置 true、结束置 false）。
    pub fn set_draining(&self, v: bool) {
        self.draining.store(v, Ordering::Relaxed);
    }

    /// 通知一次「一个运行中任务释放」（runner 终态后调用，唤醒等待排空的 upgrader）。
    pub fn notify_released(&self) {
        self.released.notify_one();
    }

    /// 等运行中任务全部终态（在途集为空）。**不设超时**——管理员可取消任务解锁。
    /// Notify 驱动即时唤醒；`DRAIN_POLL` 兜底防丢通知。
    pub async fn wait_drained(&self, in_flight: &Arc<RwLock<Vec<String>>>) {
        loop {
            if in_flight.read().await.is_empty() {
                return;
            }
            let _ = tokio::time::timeout(DRAIN_POLL, self.released.notified()).await;
        }
    }
}

/// 排空态 RAII 守卫：创建即置 draining=true，drop 即置 false——保证
/// [`Handle::perform_upgrade`] 任一退出路径（成功/各失败/退回）都清排空态，
/// 避免「忘清一处致 agent 永久排空拒收所有任务」。守卫持闸门克隆（与
/// [`DrainGate`] 共享同一内部 `Arc`），在 `perform_upgrade` 顶部创建、函数
/// 结束（含所有早返回）时 drop。
struct DrainGuard(DrainGate);

impl DrainGuard {
    /// 置排空态并返回守卫（drop 时清除）。
    fn new(gate: DrainGate) -> Self {
        gate.set_draining(true);
        Self(gate)
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.set_draining(false);
    }
}

// ============================================================
// 升级上行链路（UpgradeUplink——set_live + report + flush_pending）
// ============================================================

/// 升级状态上行链路：活体发送器 + 最新阶段缓冲。`Clone`——组合根、
/// `run_connection`（set_live / flush_pending）、upgrader Handle 各持一份共享同一内部。
///
/// 与 runner 的 `RunnerUplink` 同款：`run_connection` 每连接注入 `out_tx`，升级阶段
/// 经此单 writer 外送；断线时最新阶段存 `pending`，重连 `flush_pending` 补发
/// （「重连上报失败原因」——退回阶段在重连后可见）。阶段是「当前态」非事件，
/// 重复发送幂等。
#[derive(Clone, Default)]
pub struct UpgradeUplink {
    live: Arc<RwLock<Option<mpsc::Sender<ChannelMessage>>>>,
    pending: Arc<std::sync::Mutex<Option<UpgradeStatus>>>,
}

impl UpgradeUplink {
    /// 新建（无活体、无缓冲）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入/清除活体发送器（`run_connection` 每连接调用）。
    pub async fn set_live(&self, tx: Option<mpsc::Sender<ChannelMessage>>) {
        *self.live.write().await = tx;
    }

    /// 上报一个升级阶段：更新最新缓冲 + 在线即发；离线仅缓冲（重连补发）。
    ///
    /// 先克隆发送器出读锁再 send（与 `logbuf::forward_live` 同款）：避免持读锁
    /// 期间 `tx.send` 阻塞与 `set_live(None)` 写锁互锁。最新阶段恒已落 `pending`
    /// 缓冲（下方先写缓冲再 send），故发送失败不丢——重连 `flush_pending` 补发。
    pub async fn report(&self, phase: UpgradePhase, error: &str) {
        let status = UpgradeStatus {
            phase: phase as i32,
            error: error.to_string(),
        };
        *self.pending.lock().expect("uplink 锁") = Some(status.clone());
        let live = self.live.read().await.clone();
        if let Some(tx) = live {
            let _ = tx
                .send(ChannelMessage {
                    kind: Some(Kind::UpgradeStatus(status)),
                })
                .await;
        }
    }

    /// 重连后补发最新阶段（`run_connection` 在 `set_live(Some)` 之后调用）。逐帧经
    /// `out_tx` 外送，与日志重放 / runner 终态补发同一 writer 保写序。**不清空**
    /// 缓冲——最新阶段是当前态，每次重连都补发（幂等）。
    pub async fn flush_pending(&self, tx: &mpsc::Sender<ChannelMessage>) {
        let status = self.pending.lock().expect("uplink 锁").clone();
        if let Some(status) = status {
            let _ = tx
                .send(ChannelMessage {
                    kind: Some(Kind::UpgradeStatus(status)),
                })
                .await;
        }
    }
}

// ============================================================
// 下载缝（Downloader trait + real / stub）
// ============================================================

/// 下载错误（HTTP 传输 / 非 2xx / 读体失败）。文本原因即可，不承载结构化字段。
#[derive(Debug)]
pub struct DownloadError(pub String);

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DownloadError {}

/// 下载缝（ADR-0017：reqwest GET download_url，Bearer agent token）。注入点：
/// 生产 [`ReqwestDownloader`]（持可注入 `reqwest::Client`），测试 fake（canned
/// 字节 / 配定错误，不真联网）。返回字节即下载体（v1 全量入内存，二进制数十 MB
/// 可接受）。
#[async_trait]
pub trait Downloader: Send + Sync {
    /// 下载 `url`（已解析为绝对 URL）；`token` 存在则 `Bearer` 鉴权。返回下载体字节。
    async fn download(&self, url: &str, token: Option<&str>) -> Result<Vec<u8>, DownloadError>;
}

/// 生产下载器：reqwest GET + Bearer。`reqwest::Client` 经构造注入（可换测）。
pub struct ReqwestDownloader {
    client: reqwest::Client,
}

impl ReqwestDownloader {
    /// 以默认 reqwest client 构造（与注册面同款）。
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestDownloader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Downloader for ReqwestDownloader {
    async fn download(&self, url: &str, token: Option<&str>) -> Result<Vec<u8>, DownloadError> {
        let mut req = self.client.get(url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DownloadError(format!("HTTP 传输失败：{e}")))?;
        if !resp.status().is_success() {
            return Err(DownloadError(format!("下载返回 HTTP {}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| DownloadError(format!("读下载体失败：{e}")))?;
        Ok(bytes.to_vec())
    }
}

/// 占位下载器（`safe_stub` 默认）：返回明确错误，不联网。测试默认装配避免真下载。
struct StubDownloader;

#[async_trait]
impl Downloader for StubDownloader {
    async fn download(&self, _url: &str, _token: Option<&str>) -> Result<Vec<u8>, DownloadError> {
        Err(DownloadError("下载器未配置（safe stub）".into()))
    }
}

// ============================================================
// spawn 缝（Spawner trait + real / stub）
// ============================================================

/// spawn 失败：spawn 系统调用失败，或新进程在 grace 窗内退出（启动失败甄别）。
#[derive(Debug)]
pub struct SpawnFailure(pub String);

impl std::fmt::Display for SpawnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SpawnFailure {}

/// spawn 缝（ADR-0017「Agent 自管 spawn」）：以继承的参数与环境 spawn 换入后的
/// 当前二进制，并甄别 grace 窗内是否健康。注入点：生产 [`ProcessSpawner`]（tokio
/// spawn + grace 甄别），测试 fake（记录被 spawn 的二进制路径 + 配定结果，**不真
/// 重启进程**——AC「spawn 构造点注入断言」）。
#[async_trait]
pub trait Spawner: Send + Sync {
    /// spawn `bin`（已换入的新二进制）配 `args`（继承参数）。`Ok` = 新进程健康过 grace
    /// 窗（旧进程可退出）；`Err` = 启动失败（spawn 失败 / 窗内退出）。
    async fn spawn(&self, bin: &Path, args: Vec<String>) -> Result<(), SpawnFailure>;
}

/// 生产启动器：tokio `Command::new(bin).args(args)`（env 默认继承）+ grace 甄别。
/// spawn 后 `SPAWN_GRACE` 内退出 = 失败；存活过窗 = 健康（丢弃 child 句柄不杀，
/// `kill_on_drop=false` 默认，新进程继续；旧进程随后退出，新进程被 init 接管）。
pub struct ProcessSpawner;

impl ProcessSpawner {
    /// 新建。
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Spawner for ProcessSpawner {
    async fn spawn(&self, bin: &Path, args: Vec<String>) -> Result<(), SpawnFailure> {
        let mut child = tokio::process::Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| SpawnFailure(format!("spawn 失败：{e}")))?;
        // grace 甄别：窗内退出 = 启动失败；过窗 = 健康。
        tokio::select! {
            status = child.wait() => {
                Err(SpawnFailure(format!("新进程在 grace 窗内退出：{status:?}")))
            }
            _ = tokio::time::sleep(SPAWN_GRACE) => {
                // 健康：丢弃 child 句柄，不杀（kill_on_drop=false 默认），新进程继续。
                Ok(())
            }
        }
    }
}

/// 占位启动器（`safe_stub` 默认）：返回明确错误，不真 spawn。测试默认装配避免
/// 真重启进程（防误把测试二进制换入/spawn）。
struct StubSpawner;

#[async_trait]
impl Spawner for StubSpawner {
    async fn spawn(&self, _bin: &Path, _args: Vec<String>) -> Result<(), SpawnFailure> {
        Err(SpawnFailure("启动器未配置（safe stub）".into()))
    }
}

// ============================================================
// 升级依赖包（UpgradeDeps——downloader + spawner + current_exe）
// ============================================================

/// 升级依赖包：下载器 / 启动器 / 当前二进制路径。三者同缝注入——生产 real、
/// 测试 fake。`safe_stub` 默认让不触发升级的测试也安全（下载即败、永不 spawn）。
pub struct UpgradeDeps {
    /// 下载器缝。
    pub downloader: Arc<dyn Downloader>,
    /// 启动器缝。
    pub spawner: Arc<dyn Spawner>,
    /// 当前运行二进制路径（换入目标；生产 `std::env::current_exe`，测试临时文件）。
    pub current_exe: PathBuf,
}

impl UpgradeDeps {
    /// 生产依赖：reqwest 下载 + tokio spawn 甄别 + `std::env::current_exe`。
    pub fn real() -> Self {
        Self {
            downloader: Arc::new(ReqwestDownloader::new()),
            spawner: Arc::new(ProcessSpawner::new()),
            current_exe: std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("sisyphus-agent")),
        }
    }

    /// safe-stub 默认：占位下载/启动器（均返回明确错误、不联网不 spawn）+ 空路径。
    /// 不触发升级的测试用此默认即可安全（下载即败、永不换入/spawn）。
    pub fn safe_stub() -> Self {
        Self {
            downloader: Arc::new(StubDownloader),
            spawner: Arc::new(StubSpawner),
            current_exe: PathBuf::new(),
        }
    }
}

// ============================================================
// 本地状态 agent.json（失败计数持久化，ADR-0017）
// ============================================================

/// 本地状态（`<data>/agent.json`）：升级连续启动失败计数 + 最近失败原因 + 最近
/// 尝试的包名。持久化以兜住崩溃窗口——同一次升级崩溃后续传保留计数（仍凑满
/// 3 次后退回）；换包（新升级）则计数清零（新二进制的新尝试，不被旧计数误判）。
#[derive(Serialize, Deserialize, Default, Clone)]
struct AgentState {
    /// 连续启动失败计数（达 [`MAX_START_FAILURES`] 即退回 .old）。
    consecutive_start_failures: u32,
    /// 最近启动失败原因（退回上报用）。
    last_failure_reason: Option<String>,
    /// 最近一次升级尝试的包名（区分「同一次升级崩溃后续传」与「新升级」：
    /// 包名不同即新升级，失败计数清零——避免旧计数误判新二进制，ADR-0017）。
    last_package: Option<String>,
}

/// 读 agent.json（不存在/损坏 = 空，不阻塞升级——丢失计数仅损失观测，v1 接受）。
fn load_state(path: &Path) -> AgentState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 原子写 agent.json（同目录 tmp → rename；rename 失败清 tmp）。失败记警告不 panic
/// ——状态是记账，写失败升级仍可继续（下次变更再写）。
fn save_state(path: &Path, state: &AgentState) {
    let text = match serde_json::to_string(state) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "agent.json 序列化失败");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &text).is_err() {
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ============================================================
// 原子换入 / 退回（纯 FS 操作）
// ============================================================

/// 换入失败（rename 失败等）。文本原因。
#[derive(Debug)]
pub struct SwapError(pub String);

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SwapError {}

/// `.old` 路径：`<bin>.old`（与当前二进制同目录同级）。
fn old_path(current: &Path) -> PathBuf {
    let name = current
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    current
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{name}{OLD_SUFFIX}"))
}

/// 下载临时文件路径：`<bin>.dl-tmp`（同目录同卷，rename 原子）。
fn dl_tmp_path(current: &Path) -> PathBuf {
    let name = current
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    current
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{name}{DL_TMP_SUFFIX}"))
}

/// 写下载体到同目录临时文件，返回其路径。失败 = [`SwapError`]。
fn write_dl_tmp(current: &Path, bytes: &[u8]) -> Result<PathBuf, SwapError> {
    let tmp = dl_tmp_path(current);
    std::fs::write(&tmp, bytes).map_err(|e| SwapError(format!("写下载临时文件失败：{e}")))?;
    Ok(tmp)
}

/// 原子换入（ADR-0017）：先清残留 `.old`（best-effort——Windows rename 不覆盖
/// 现存文件，故先删；删不动则下一步 rename 会失败 → 安全中止），再 rename
/// 当前 → `.old`（Unix 覆盖现存 .old；Windows 运行中 exe 可 rename），再 rename
/// 临时 → 当前。第 3 步失败时回退：rename `.old` → 当前（不留空窗）。任一步
/// 不可恢复即 `Err`（调用方保持旧版本继续跑——此时当前可能已被挪到 .old，
/// 回退把 .old 挪回）。
fn swap_binary(current: &Path, temp: &Path) -> Result<(), SwapError> {
    let old = old_path(current);
    // 1. 清残留 .old（best-effort）。
    let _ = std::fs::remove_file(&old);
    // 2. 当前 → .old。
    if let Err(e) = std::fs::rename(current, &old) {
        return Err(SwapError(format!("rename 当前→.old 失败：{e}")));
    }
    // 3. 临时 → 当前；失败回退 .old → 当前。
    if let Err(e) = std::fs::rename(temp, current) {
        let _ = std::fs::rename(&old, current);
        return Err(SwapError(format!("rename 新→当前 失败：{e}")));
    }
    Ok(())
}

/// 退回 `.old`（连续启动失败后）：删当前（坏的新二进制，未运行可删）+ rename
/// `.old` → 当前。退回后运行中的旧进程（在 Unix 持旧 inode、在 Windows 其路径
/// 被 rename 回当前）继续，重连上报失败原因。
fn rollback_binary(current: &Path) -> Result<(), SwapError> {
    let old = old_path(current);
    let _ = std::fs::remove_file(current);
    std::fs::rename(&old, current)
        .map_err(|e| SwapError(format!("rollback rename .old→当前 失败：{e}")))
}

// ============================================================
// 纯逻辑助手
// ============================================================

/// sha256 摘要 → 小写 hex。
fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for &b in digest.iter() {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// sha256 比对（大小写不敏感——服务端 hex 大小写不可假设）。
fn sha_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected.trim())
}

/// 解析下载 URL：绝对（`http://`/`https://`）原样；相对（`/`-开头或裸路径）拼
/// `api_url`；无 `api_url` 则原样返回（GET 时自败）。与注册面 `register_url` 同款。
fn resolve_download_url(api_url: Option<&str>, download_url: &str) -> String {
    if download_url.starts_with("http://") || download_url.starts_with("https://") {
        return download_url.to_string();
    }
    match api_url {
        Some(base) => {
            let path = if download_url.starts_with('/') {
                download_url.to_string()
            } else {
                format!("/{download_url}")
            };
            format!("{}{path}", base.trim_end_matches('/'))
        }
        None => download_url.to_string(),
    }
}

// ============================================================
// 句柄（下行分派循环 owner）
// ============================================================

/// upgrader 句柄：下行接收端 + 上行链路 + 排空闸门 + 在途集 + 凭据 + 升级依赖 +
/// agent.json 路径 + 退出信号 + 收帧观测。`run` 消费 self；`perform_upgrade` 是
/// 单次升级全流程（可测性：测试直驱构造句柄调用）。
pub struct Handle {
    rx: mpsc::Receiver<ChannelMessage>,
    uplink: UpgradeUplink,
    gate: DrainGate,
    in_flight: Arc<RwLock<Vec<String>>>,
    /// Agent 长期凭据（下载 Bearer）。
    token: Option<String>,
    /// Server REST 基址（相对 download_url 拼接用）。
    api_url: Option<String>,
    deps: UpgradeDeps,
    agent_json_path: PathBuf,
    /// 升级成功后置位 → `Agent::run` 退出（旧进程退出，新进程接管）。`None` =
    /// 测试模式（不真退出，便于断言）。
    exit_tx: Option<watch::Sender<bool>>,
    receipts: ReceiptLog,
}

impl Handle {
    /// 装配句柄。`exit_tx` 为 `None` 时不真退出（测试用）。
    #[allow(clippy::too_many_arguments)] // 下行/上行/闸门/在途/凭据/依赖/状态/退出各一参，语义独立
    pub fn new(
        rx: mpsc::Receiver<ChannelMessage>,
        uplink: UpgradeUplink,
        gate: DrainGate,
        in_flight: Arc<RwLock<Vec<String>>>,
        token: Option<String>,
        api_url: Option<String>,
        deps: UpgradeDeps,
        agent_json_path: PathBuf,
        exit_tx: Option<watch::Sender<bool>>,
        receipts: ReceiptLog,
    ) -> Self {
        Self {
            rx,
            uplink,
            gate,
            in_flight,
            token,
            api_url,
            deps,
            agent_json_path,
            exit_tx,
            receipts,
        }
    }

    /// 下行循环：收 [`UpgradeCommand`] 即驱动一次升级全流程。rx 关闭即退出。
    pub async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            match msg.kind {
                Some(Kind::Upgrade(cmd)) => {
                    self.receipts.lock().expect("观测锁").push("upgrade".into());
                    self.perform_upgrade(cmd).await;
                }
                _ => {
                    self.receipts.lock().expect("观测锁").push("other".into());
                    tracing::warn!(?msg, "upgrader 收到非升级指令，忽略");
                }
            }
        }
    }

    /// 一次升级全流程：排空 → 下载 → sha256 校验 → 原子换入 → spawn（重试 3 次）
    /// → 连续 3 次启动失败退回 `.old`。任一步失败保持旧版本、上报错误、继续跑。
    /// `pub(crate)` 供单元测试直驱。
    pub(crate) async fn perform_upgrade(&mut self, cmd: UpgradeCommand) {
        // 1. 排空：RAII 守卫置 draining（runner 即拒收新任务），函数任一退出都清。
        //    守卫先于 Draining 上报——保证观测到 Draining 时 draining 已生效。
        let _drain = DrainGuard::new(self.gate.clone());
        self.uplink.report(UpgradePhase::UpgradeDraining, "").await;
        self.gate.wait_drained(&self.in_flight).await;

        // 2. 下载。
        self.uplink
            .report(UpgradePhase::UpgradeDownloading, "")
            .await;
        let url = resolve_download_url(self.api_url.as_deref(), &cmd.download_url);
        let bytes = match self
            .deps
            .downloader
            .download(&url, self.token.as_deref())
            .await
        {
            Ok(b) => b,
            Err(e) => {
                self.uplink
                    .report(UpgradePhase::UpgradeDownloading, &format!("下载失败：{e}"))
                    .await;
                return;
            }
        };

        // 3. sha256 校验：不符即弃、保持旧版、上报、继续。
        let actual = sha256_hex(&bytes);
        if !sha_matches(&actual, &cmd.sha256) {
            self.uplink
                .report(
                    UpgradePhase::UpgradeDownloading,
                    &format!("sha256 校验失败：期望 {} 实得 {}", cmd.sha256, actual),
                )
                .await;
            return;
        }

        // 4. 原子换入：写临时文件 → rename 当前→.old、临时→当前。
        self.uplink.report(UpgradePhase::UpgradeSwapping, "").await;
        let temp = match write_dl_tmp(&self.deps.current_exe, &bytes) {
            Ok(p) => p,
            Err(e) => {
                self.uplink
                    .report(
                        UpgradePhase::UpgradeSwapping,
                        &format!("写临时文件失败：{e}"),
                    )
                    .await;
                return;
            }
        };
        if let Err(e) = swap_binary(&self.deps.current_exe, &temp) {
            let _ = std::fs::remove_file(&temp);
            self.uplink
                .report(UpgradePhase::UpgradeSwapping, &format!("原子换入失败：{e}"))
                .await;
            return;
        }

        // 5. spawn 新进程（继承参数与环境）+ 甄别；连续 3 次启动失败退回 .old。
        self.uplink
            .report(UpgradePhase::UpgradeRestarting, "")
            .await;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut state = load_state(&self.agent_json_path);
        // 新升级（包名不同）→ 失败计数清零（ADR-0017：连续 3 次是同一次升级内
        // 的计数；崩溃后同包续传保留计数，换包则从头计，避免旧计数误判新二进制）。
        if state.last_package.as_deref() != Some(&cmd.package_name) {
            state.consecutive_start_failures = 0;
            state.last_failure_reason = None;
            state.last_package = Some(cmd.package_name.clone());
            save_state(&self.agent_json_path, &state);
        }
        loop {
            match self
                .deps
                .spawner
                .spawn(&self.deps.current_exe, args.clone())
                .await
            {
                Ok(()) => {
                    state.consecutive_start_failures = 0;
                    state.last_failure_reason = None;
                    save_state(&self.agent_json_path, &state);
                    self.uplink
                        .report(UpgradePhase::UpgradeRestarting, "新进程已启动，旧进程退出")
                        .await;
                    if let Some(tx) = &self.exit_tx {
                        let _ = tx.send(true);
                    }
                    return;
                }
                Err(e) => {
                    state.consecutive_start_failures += 1;
                    state.last_failure_reason = Some(e.to_string());
                    save_state(&self.agent_json_path, &state);
                    if state.consecutive_start_failures >= MAX_START_FAILURES {
                        let reason = e.to_string();
                        match rollback_binary(&self.deps.current_exe) {
                            Ok(()) => {
                                self.uplink
                                    .report(
                                        UpgradePhase::UpgradeFallback,
                                        &format!(
                                            "连续 {MAX_START_FAILURES} 次启动失败，已退回 .old：{reason}"
                                        ),
                                    )
                                    .await;
                            }
                            Err(re) => {
                                self.uplink
                                    .report(
                                        UpgradePhase::UpgradeFallback,
                                        &format!("退回 .old 失败：{re}（原启动失败：{reason}）"),
                                    )
                                    .await;
                            }
                        }
                        state.consecutive_start_failures = 0;
                        state.last_failure_reason = None;
                        save_state(&self.agent_json_path, &state);
                        return;
                    }
                    self.uplink
                        .report(
                            UpgradePhase::UpgradeRestarting,
                            &format!(
                                "启动失败（{}/{MAX_START_FAILURES}）：{e}",
                                state.consecutive_start_failures
                            ),
                        )
                        .await;
                    // 重试同一新二进制（count < 3）。
                }
            }
        }
    }
}

// ============================================================
// 单元测试（纯逻辑 + 真实 FS + 注入 fake；TDD 红→绿）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// 下载器观测：记录收到的 (url, token) 序列。
    type DlSeen = Arc<StdMutex<Vec<(String, Option<String>)>>>;
    /// 启动器观测：记录被 spawn 的二进制路径序列。
    type SpawnSeen = Arc<StdMutex<Vec<PathBuf>>>;

    // --- 纯逻辑 ---

    /// 绝对/相对 URL 解析。
    #[test]
    fn resolve_download_url_absolute_passthrough_and_relative_join() {
        assert_eq!(
            resolve_download_url(Some("http://api:8080"), "https://get/file"),
            "https://get/file",
            "绝对 https 原样"
        );
        assert_eq!(
            resolve_download_url(Some("http://api:8080"), "http://get/file"),
            "http://get/file",
            "绝对 http 原样"
        );
        assert_eq!(
            resolve_download_url(Some("http://api:8080"), "/api/v1/pkg/download"),
            "http://api:8080/api/v1/pkg/download",
            "相对 /-开头拼 api_url"
        );
        assert_eq!(
            resolve_download_url(Some("http://api:8080/"), "api/v1/pkg"),
            "http://api:8080/api/v1/pkg",
            "相对裸路径补 / 拼去尾斜杠 api_url"
        );
        assert_eq!(
            resolve_download_url(None, "/api/v1/pkg"),
            "/api/v1/pkg",
            "无 api_url 原样返回（GET 时自败）"
        );
    }

    /// sha256 编码 + 比对（已知向量：空串摘要）。
    #[test]
    fn sha256_hex_and_match_known_vector() {
        // sha256(b"") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(sha_matches(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        // 大小写不敏感 + 容忍前后空白。
        assert!(sha_matches("ABC123", "  abc123  "));
        assert!(!sha_matches("abc", "def"));
    }

    /// .old / dl-tmp 路径构造。
    #[test]
    fn old_and_tmp_paths_are_sibling_suffixed() {
        let cur = Path::new("/opt/sisyphus-agent/sisyphus-agent");
        assert_eq!(
            old_path(cur),
            Path::new("/opt/sisyphus-agent/sisyphus-agent.old")
        );
        assert_eq!(
            dl_tmp_path(cur),
            Path::new("/opt/sisyphus-agent/sisyphus-agent.dl-tmp")
        );
        // Windows 形态。
        let cur = Path::new("C:\\Program Files\\sisyphus\\agent.exe");
        assert_eq!(
            old_path(cur),
            Path::new("C:\\Program Files\\sisyphus\\agent.exe.old")
        );
    }

    /// 排空闸门：置位/读取 + wait_drained 在途集空即返回。
    #[tokio::test]
    async fn drain_gate_set_and_wait_when_empty() {
        let gate = DrainGate::new();
        assert!(!gate.is_draining());
        gate.set_draining(true);
        assert!(gate.is_draining());
        let in_flight = Arc::new(RwLock::new(Vec::<String>::new()));
        gate.wait_drained(&in_flight).await;
        gate.set_draining(false);
    }

    /// 排空闸门：在途集非空 → wait_drained 阻塞；释放 + 通知后返回。
    #[tokio::test]
    async fn drain_gate_waits_until_inflight_drains() {
        let gate = DrainGate::new();
        let in_flight: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec!["job-1".into()]));
        let gate_c = gate.clone();
        let in_flight_c = in_flight.clone();
        let wait = tokio::spawn(async move { gate_c.wait_drained(&in_flight_c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!wait.is_finished(), "在途非空应阻塞");
        in_flight.write().await.clear();
        gate.notify_released();
        wait.await.expect("wait_drained 应在释放后返回");
    }

    /// 升级上行链路：在线 report 即发 + 缓冲最新；离线 report 仅缓冲；flush_pending
    /// 补发最新。
    #[tokio::test]
    async fn upgrade_uplink_live_send_pending_backfill() {
        let uplink = UpgradeUplink::new();
        let (tx, mut rx) = mpsc::channel::<ChannelMessage>(8);
        uplink.set_live(Some(tx)).await;
        uplink.report(UpgradePhase::UpgradeDraining, "").await;
        let msg = rx.recv().await.expect("在线应即发");
        match msg.kind {
            Some(Kind::UpgradeStatus(s)) => {
                assert_eq!(s.phase, UpgradePhase::UpgradeDraining as i32);
                assert!(s.error.is_empty());
            }
            other => panic!("期望 UpgradeStatus，得 {other:?}"),
        }
        // 离线：report 仅缓冲。
        uplink.set_live(None).await;
        uplink
            .report(UpgradePhase::UpgradeDownloading, "下载失败：x")
            .await;
        // 重连：flush_pending 补发最新。
        let (tx2, mut rx2) = mpsc::channel::<ChannelMessage>(8);
        uplink.set_live(Some(tx2.clone())).await;
        uplink.flush_pending(&tx2).await;
        let msg = rx2.recv().await.expect("补发最新阶段");
        match msg.kind {
            Some(Kind::UpgradeStatus(s)) => {
                assert_eq!(s.phase, UpgradePhase::UpgradeDownloading as i32);
                assert_eq!(s.error, "下载失败：x");
            }
            other => panic!("期望补发 UpgradeStatus，得 {other:?}"),
        }
    }

    /// 原子换入 + 退回（真实 fs）：当前有旧字节、临时有新字节 → 换入后当前=新、
    /// .old=旧；退回后当前=旧、.old 消失。
    #[test]
    fn swap_and_rollback_roundtrip_on_real_fs() {
        let dir = tempfile::tempdir().expect("临时目录");
        let cur = dir.path().join("agent.bin");
        std::fs::write(&cur, b"OLD").expect("写旧");
        let tmp = dir.path().join("agent.bin.dl-tmp");
        std::fs::write(&tmp, b"NEW").expect("写新");

        swap_binary(&cur, &tmp).expect("换入");
        assert_eq!(std::fs::read(&cur).unwrap(), b"NEW", "当前已换为新");
        assert_eq!(
            std::fs::read(old_path(&cur)).unwrap(),
            b"OLD",
            "旧二进制保留为 .old"
        );
        assert!(!tmp.exists(), "临时文件已被 rename 走");

        rollback_binary(&cur).expect("退回");
        assert_eq!(std::fs::read(&cur).unwrap(), b"OLD", "退回后当前=旧");
        assert!(!old_path(&cur).exists(), "退回后 .old 已挪回当前");
    }

    /// 换入内部回退：缺临时文件让「临时→当前」失败 → .old 被挪回当前，不留空窗。
    #[test]
    fn swap_internal_rollback_restores_current_on_failure() {
        let dir = tempfile::tempdir().expect("临时目录");
        let cur = dir.path().join("agent.bin");
        std::fs::write(&cur, b"OLD").expect("写旧");
        let tmp = dir.path().join("agent.bin.dl-tmp");
        let err = swap_binary(&cur, &tmp).expect_err("缺临时应失败");
        assert!(err.to_string().contains("rename 新→当前"));
        assert_eq!(std::fs::read(&cur).unwrap(), b"OLD", "回退后当前仍是旧");
        assert!(!old_path(&cur).exists(), "回退后 .old 不残留");
    }

    /// agent.json 读写往返。
    #[test]
    fn agent_state_persists_and_reloads() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("agent.json");
        assert_eq!(load_state(&path).consecutive_start_failures, 0, "缺省 0");
        let state = AgentState {
            consecutive_start_failures: 2,
            last_failure_reason: Some("bad bin".into()),
            last_package: Some("agent-1.0.1".into()),
        };
        save_state(&path, &state);
        let loaded = load_state(&path);
        assert_eq!(loaded.consecutive_start_failures, 2);
        assert_eq!(loaded.last_failure_reason.as_deref(), Some("bad bin"));
        assert_eq!(loaded.last_package.as_deref(), Some("agent-1.0.1"));
    }

    // --- fake 下载器 / 启动器（注入缝）---

    /// fake 下载器：配定字节或错误，记录收到的 url + token。
    struct FakeDownloader {
        bytes: Result<Vec<u8>, String>,
        seen: DlSeen,
    }
    #[async_trait]
    impl Downloader for FakeDownloader {
        async fn download(&self, url: &str, token: Option<&str>) -> Result<Vec<u8>, DownloadError> {
            self.seen
                .lock()
                .expect("锁")
                .push((url.to_string(), token.map(str::to_string)));
            self.bytes.clone().map_err(DownloadError)
        }
    }

    /// fake 启动器：配定结果序列（每次 spawn 取一个），记录被 spawn 的二进制路径。
    struct FakeSpawner {
        results: Vec<Result<(), String>>,
        next: Arc<StdMutex<usize>>,
        recorded: SpawnSeen,
    }
    #[async_trait]
    impl Spawner for FakeSpawner {
        async fn spawn(&self, bin: &Path, _args: Vec<String>) -> Result<(), SpawnFailure> {
            self.recorded.lock().expect("锁").push(bin.to_path_buf());
            let mut idx = self.next.lock().expect("锁");
            let i = *idx;
            *idx += 1;
            match self.results.get(i) {
                Some(Ok(())) => Ok(()),
                Some(Err(e)) => Err(SpawnFailure(e.clone())),
                None => Err(SpawnFailure("fake: 结果序列耗尽".into())),
            }
        }
    }

    /// 测试装配：真实临时二进制 + fake 下载/启动 + 在途集 + 闸门 + 上行（已 set_live）。
    struct Fixture {
        handle: Handle,
        dl_seen: DlSeen,
        spawn_seen: SpawnSeen,
        cur: PathBuf,
        up_rx: mpsc::Receiver<ChannelMessage>,
        exit_rx: Option<watch::Receiver<bool>>,
        agent_json: PathBuf,
        in_flight: Arc<RwLock<Vec<String>>>,
        gate: DrainGate,
    }

    /// 装配。`dl_bytes` = 下载器配定字节/错误；`spawn_results` = 启动器结果序列；
    /// `in_flight` = 初始在途集；`exit` = 是否装配退出信号 watch。
    async fn fixture(
        dir: &Path,
        dl_bytes: Result<Vec<u8>, String>,
        spawn_results: Vec<Result<(), String>>,
        in_flight: Vec<String>,
        exit: bool,
    ) -> Fixture {
        let cur = dir.join("agent.bin");
        std::fs::write(&cur, b"OLD").expect("写旧二进制");
        let dl_seen = Arc::new(StdMutex::new(Vec::new()));
        let spawn_seen = Arc::new(StdMutex::new(Vec::new()));
        let downloader: Arc<dyn Downloader> = Arc::new(FakeDownloader {
            bytes: dl_bytes,
            seen: dl_seen.clone(),
        });
        let spawner: Arc<dyn Spawner> = Arc::new(FakeSpawner {
            results: spawn_results,
            next: Arc::new(StdMutex::new(0)),
            recorded: spawn_seen.clone(),
        });
        let (exit_tx, exit_rx) = if exit {
            let (tx, rx) = watch::channel(false);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let uplink = UpgradeUplink::new();
        let (up_tx, up_rx) = mpsc::channel::<ChannelMessage>(64);
        // 上行活体注入：report 在线即发到 up_rx。
        uplink.set_live(Some(up_tx)).await;
        let in_flight = Arc::new(RwLock::new(in_flight));
        let gate = DrainGate::new();
        let agent_json = dir.join("agent.json");
        let (_dispatch_tx, rx_dispatch) = mpsc::channel::<ChannelMessage>(4);
        let handle = Handle::new(
            rx_dispatch,
            uplink,
            gate.clone(),
            in_flight.clone(),
            Some("sisa_token".into()),
            Some("http://api:8080".into()),
            UpgradeDeps {
                downloader,
                spawner,
                current_exe: cur.clone(),
            },
            agent_json.clone(),
            exit_tx,
            Arc::new(StdMutex::new(Vec::new())),
        );
        Fixture {
            handle,
            dl_seen,
            spawn_seen,
            cur,
            up_rx,
            exit_rx,
            agent_json,
            in_flight,
            gate,
        }
    }

    /// AC：升级全流程成功——下载（sha256 符）→ 换入 → spawn 健康 → 旧进程退出。
    #[tokio::test]
    async fn perform_upgrade_success_swaps_and_exits() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(dir.path(), Ok(bytes), vec![Ok(())], vec![], true).await;

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(std::fs::read(&fx.cur).unwrap(), b"NEW-BIN", "当前已换新");
        assert_eq!(
            std::fs::read(old_path(&fx.cur)).unwrap(),
            b"OLD",
            ".old 保留旧"
        );
        let spawns = fx.spawn_seen.lock().expect("锁").clone();
        assert_eq!(
            spawns,
            vec![fx.cur.clone()],
            "spawn 的是换入后的新路径二进制"
        );
        assert_eq!(
            load_state(&fx.agent_json).consecutive_start_failures,
            0,
            "成功后失败计数清零"
        );
        assert!(
            fx.exit_rx.as_ref().is_some_and(|r| *r.borrow()),
            "成功后应置退出信号"
        );
        let dl = fx.dl_seen.lock().expect("锁").clone();
        assert_eq!(dl.len(), 1);
        assert_eq!(dl[0].0, "http://get/pkg");
        assert_eq!(dl[0].1.as_deref(), Some("sisa_token"));
    }

    /// AC：sha256 校验失败——弃、保持旧版、上报错误、继续跑（不换入、不 spawn）。
    #[tokio::test]
    async fn perform_upgrade_sha_mismatch_keeps_old() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let wrong_sha = sha256_hex(b"DIFFERENT");
        let mut fx = fixture(dir.path(), Ok(bytes), vec![Ok(())], vec![], false).await;

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: wrong_sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(std::fs::read(&fx.cur).unwrap(), b"OLD", "校验失败保持旧版");
        assert!(!old_path(&fx.cur).exists(), "未换入，无 .old");
        assert!(
            fx.spawn_seen.lock().expect("锁").is_empty(),
            "未换入，不 spawn"
        );
        assert!(has_phase(
            &mut fx.up_rx,
            UpgradePhase::UpgradeDownloading,
            "sha256 校验失败"
        ));
    }

    /// AC：下载失败——保持旧版、上报错误、继续跑。
    #[tokio::test]
    async fn perform_upgrade_download_failure_keeps_old() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut fx = fixture(
            dir.path(),
            Err("网络不可达".into()),
            vec![Ok(())],
            vec![],
            false,
        )
        .await;

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: "deadbeef".into(),
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(std::fs::read(&fx.cur).unwrap(), b"OLD", "下载失败保持旧版");
        assert!(fx.spawn_seen.lock().expect("锁").is_empty(), "不 spawn");
        assert!(has_phase(
            &mut fx.up_rx,
            UpgradePhase::UpgradeDownloading,
            "下载失败"
        ));
    }

    /// AC：连续 3 次启动失败自动换回 .old——退回后当前=旧、.old 消失、Fallback 上报。
    #[tokio::test]
    async fn perform_upgrade_three_start_failures_roll_back() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(
            dir.path(),
            Ok(bytes),
            vec![Err("boom".into()), Err("boom".into()), Err("boom".into())],
            vec![],
            false,
        )
        .await;

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(
            std::fs::read(&fx.cur).unwrap(),
            b"OLD",
            "3 次失败后退回旧版"
        );
        assert!(!old_path(&fx.cur).exists(), "退回后 .old 已挪回当前");
        assert_eq!(fx.spawn_seen.lock().expect("锁").len(), 3, "重试 3 次");
        assert!(has_phase(
            &mut fx.up_rx,
            UpgradePhase::UpgradeFallback,
            "退回"
        ));
        assert_eq!(load_state(&fx.agent_json).consecutive_start_failures, 0);
    }

    /// AC：1 次启动失败后第 2 次成功——重试成功，不退回，旧进程退出。
    #[tokio::test]
    async fn perform_upgrade_retry_succeeds_within_three() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(
            dir.path(),
            Ok(bytes),
            vec![Err("boom".into()), Ok(())],
            vec![],
            true,
        )
        .await;

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(
            std::fs::read(&fx.cur).unwrap(),
            b"NEW-BIN",
            "重试成功已换新"
        );
        assert_eq!(
            fx.spawn_seen.lock().expect("锁").len(),
            2,
            "重试 1 次后成功"
        );
        assert!(
            fx.exit_rx.as_ref().is_some_and(|r| *r.borrow()),
            "成功后置退出信号"
        );
    }

    /// AC（ADR-0017 失败计数持久化语义）：换包（新升级）→ 失败计数清零——
    /// 预置 agent.json 计数=2 + last_package=A，对包 B 升级配 3 次启动失败，
    /// 断言 spawn 重试满 3 次（计数被清零从头计）才退回，而非沿用旧计数 1 次即退。
    #[tokio::test]
    async fn perform_upgrade_new_package_resets_failure_count() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(
            dir.path(),
            Ok(bytes),
            vec![Err("boom".into()), Err("boom".into()), Err("boom".into())],
            vec![],
            false,
        )
        .await;
        // 预置：上一次升级（包 A）崩溃在重试中途，计数=2 已持久化。
        save_state(
            &fx.agent_json,
            &AgentState {
                consecutive_start_failures: 2,
                last_failure_reason: Some("old crash".into()),
                last_package: Some("agent-A".into()),
            },
        );
        // 新升级：包 B（与 A 不同）→ 计数应清零从头计。
        let cmd = UpgradeCommand {
            package_name: "agent-B".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        // 换包清零后重试满 3 次（而非沿用 2 → 1 次即退）。
        assert_eq!(
            fx.spawn_seen.lock().expect("锁").len(),
            3,
            "换包清零后从头计，重试满 3 次"
        );
        assert!(has_phase(
            &mut fx.up_rx,
            UpgradePhase::UpgradeFallback,
            "退回"
        ));
        // 退回后计数清零、last_package 仍记 B。
        let state = load_state(&fx.agent_json);
        assert_eq!(state.consecutive_start_failures, 0, "退回后计数清零");
        assert_eq!(state.last_package.as_deref(), Some("agent-B"));
    }

    /// AC（同包续传保留计数）：同包再次升级时沿用已持久化的失败计数——
    /// 预置计数=2 + last_package=A，对同包 A 升级配 1 次失败，断言仅 1 次 spawn
    /// 即触发退回（沿用计数 2 → 3），证明崩溃续传语义。
    #[tokio::test]
    async fn perform_upgrade_same_package_keeps_failure_count() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(
            dir.path(),
            Ok(bytes),
            vec![Err("boom".into())],
            vec![],
            false,
        )
        .await;
        save_state(
            &fx.agent_json,
            &AgentState {
                consecutive_start_failures: 2,
                last_failure_reason: Some("old crash".into()),
                last_package: Some("agent-A".into()),
            },
        );
        // 同包 A 再升（崩溃续传）→ 沿用计数 2，1 次失败即达 3 退回。
        let cmd = UpgradeCommand {
            package_name: "agent-A".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        fx.handle.perform_upgrade(cmd).await;

        assert_eq!(
            fx.spawn_seen.lock().expect("锁").len(),
            1,
            "同包续传沿用计数 2，1 次失败即退回"
        );
        assert!(has_phase(
            &mut fx.up_rx,
            UpgradePhase::UpgradeFallback,
            "退回"
        ));
    }

    /// AC：排空——在途非空时 perform_upgrade 阻塞在排空；释放 + 通知后继续下载，
    /// 升级结束后排空态清除。
    #[tokio::test]
    async fn perform_upgrade_drains_inflight_before_download() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bytes = b"NEW-BIN".to_vec();
        let sha = sha256_hex(&bytes);
        let mut fx = fixture(
            dir.path(),
            Ok(bytes),
            vec![Ok(())],
            vec!["job-1".into()],
            false,
        )
        .await;
        // 取出共享观测 + 在途集 + 闸门克隆（handle 将被 move 进 spawn 任务）。
        let dl_seen = fx.dl_seen.clone();
        let gate = fx.gate.clone();
        let in_flight = fx.in_flight.clone();
        let mut up_rx = std::mem::replace(&mut fx.up_rx, mpsc::channel::<ChannelMessage>(1).1);
        let mut handle = std::mem::replace(&mut fx.handle, stub_handle());

        let cmd = UpgradeCommand {
            package_name: "agent-1.0.1".into(),
            sha256: sha,
            download_url: "http://get/pkg".into(),
        };
        let perf = tokio::spawn(async move { handle.perform_upgrade(cmd).await });
        // 排空中：Draining 上报已发，但下载未发生（在途非空阻塞）。
        wait_for_phase(&mut up_rx, UpgradePhase::UpgradeDraining, "").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            dl_seen.lock().expect("锁").is_empty(),
            "在途非空时应阻塞排空，不下载"
        );
        assert!(gate.is_draining(), "排空态置位");
        // 释放在途 → 通知 → 排空完成 → 下载发生 → 升级结束。
        in_flight.write().await.clear();
        gate.notify_released();
        perf.await.expect("perform_upgrade 完成");
        assert!(!dl_seen.lock().expect("锁").is_empty(), "释放后下载发生");
        assert!(!gate.is_draining(), "升级结束后排空态清除");
    }

    // --- 测试小工具 ---

    /// 占位句柄：`std::mem::replace` 把 `fx.handle` 顶替出来时用（不真用）。
    fn stub_handle() -> Handle {
        let (_tx, rx) = mpsc::channel::<ChannelMessage>(1);
        Handle::new(
            rx,
            UpgradeUplink::new(),
            DrainGate::new(),
            Arc::new(RwLock::new(Vec::new())),
            None,
            None,
            UpgradeDeps::safe_stub(),
            PathBuf::new(),
            None,
            Arc::new(StdMutex::new(Vec::new())),
        )
    }

    async fn wait_for_phase(
        rx: &mut mpsc::Receiver<ChannelMessage>,
        phase: UpgradePhase,
        contains: &str,
    ) {
        for _ in 0..100 {
            if let Ok(msg) = rx.try_recv()
                && let Some(Kind::UpgradeStatus(s)) = msg.kind
                && s.phase == phase as i32
                && s.error.contains(contains)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("未在 1s 内等到阶段 {phase:?} 含 {contains:?}");
    }

    fn has_phase(
        rx: &mut mpsc::Receiver<ChannelMessage>,
        phase: UpgradePhase,
        contains: &str,
    ) -> bool {
        while let Ok(msg) = rx.try_recv() {
            if let Some(Kind::UpgradeStatus(s)) = msg.kind
                && s.phase == phase as i32
                && s.error.contains(contains)
            {
                return true;
            }
        }
        false
    }
}

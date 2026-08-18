//! Agent 通道（gRPC 连接管理，ADR-0007/0008/0017/0019；票 B3-T1）。
//!
//! 从 B1 最小握手升级为可长期运行的连接基座：
//! - **token 认证握手**：`Authorization: Bearer <sisa_>` metadata（Server 侧
//!   grpc 已实现）；系统标签（os/arch 静态 + 容器探测占位）随连接 metadata
//!   上报（ADR-0008：连接面事实，不可手编）。
//! - **版本窗口**：Server 过新拒连并明确报错（ADR-0010/0017：Agent 版本
//!   不得高于 Server）；「过旧任务面拒连、升级面保留」是 Server 侧策略
//!   （grpc.rs 已留「任务面细化归后续批次」），Agent 侧分派 gating 随任务
//!   面批次细化。
//! - **15s 心跳 + 磁盘占用上报**（ADR-0007/0019）：卷级 free/total（statvfs /
//!   GetDiskFreeSpaceExW）+ 缓存记账（占位 0，随 cache 批次）+ 工作区采样
//!   （占位 0，随 workspace 批次）。
//! - **指数退避重连**：1s 起、×2、上限 60s、加抖动、永久重试不自杀
//!   （[`Backoff`]）。每次重连 = 新握手 + 认证 + [`Kind::JobReported`] 在途
//!   上报 + 标签刷新（metadata 每次连接重建）。
//! - **分派骨架**：单 reader 循环收下行帧，按类型投递各模块 mpsc
//!   （[`Dispatch`]）；单 writer 串行转发全部上行帧（保写序）。

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sisyphus_proto::agent::{
    ChannelMessage, DiskUsage, Handshake, Heartbeat, JobReported, Version,
    agent_channel_client::AgentChannelClient, channel_message::Kind,
};
use sisyphus_proto::version;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;

use crate::logbuf::LogBuffer;
use crate::workspace::Workspace;

/// 心跳间隔（ADR-0007：15s 一报；与 Server 侧 `HEARTBEAT_INTERVAL_MS` 同值）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// 上行帧邮箱容量（心跳/ack/状态/日志/在途/列表响应共走一根通道；缓冲满
/// 即背压等待，不丢帧——单 writer 纪律）。
const OUTBOUND_CAPACITY: usize = 64;
/// 分派通道容量（各模块下行缓冲；满即 reader 背压，不丢下行帧）。
pub const DISPATCH_CAPACITY: usize = 32;

/// Agent 侧版本（ADR-0010：与 Server 同版本成对发布）。
pub fn agent_version() -> Version {
    version::VERSION
}

/// 主机名（仅标识，尽力而为：Unix 取 HOSTNAME、Windows 取 COMPUTERNAME）。
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

// ============================================================
// 版本窗口
// ============================================================

/// 版本窗口裁决（Agent 侧，ADR-0010/0017）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionVerdict {
    /// Server 不高于本地：窗口内，正常连入。
    Compatible,
    /// Server 过新（高于本地）：拒连并明确报错，退避重试等运维介入。
    ServerTooNew,
}

/// 版本窗口判定（B1 语义保留：对端任一 semver 段更大即过新）。
pub fn version_window(server: &Version, local: &Version) -> VersionVerdict {
    if version::peer_too_new(server, local) {
        VersionVerdict::ServerTooNew
    } else {
        VersionVerdict::Compatible
    }
}

/// 版本号的显示形态（`major.minor.patch`）。
fn fmt_version(v: &Version) -> String {
    format!("{}.{}.{}", v.major, v.minor, v.patch)
}

// ============================================================
// 系统标签（连接面事实，ADR-0008）
// ============================================================

/// 系统标签 metadata 头名（与 Server 侧 grpc.rs 常量同形；取值域见
/// ADR-0008，不可手编）。
pub const META_OS: &str = "x-sisyphus-os";
/// 架构标签 metadata 头名。
pub const META_ARCH: &str = "x-sisyphus-arch";
/// 容器标签 metadata 头名（探测成功才置 `docker`，失败即不置）。
pub const META_CONTAINER: &str = "x-sisyphus-container";

/// 容器探测周期（ADR-0018：周期 `docker version`；默认 60s）。
pub const CONTAINER_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// 系统标签源：os/arch 静态 + 容器探测动态（探测结果随每次连接刷新）。
/// `probe_handle` 暴露容器探测句柄，供 [`crate::Agent::run`] spawn 周期探测
/// （静态源返回 `None`，不探测）。
pub trait LabelSource: Send + Sync {
    /// 当前系统标签（metadata 头名 → 值）。
    fn labels(&self) -> Vec<(&'static str, String)>;
    /// 容器探测刷新句柄（[`PlatformLabels`] 返回 `Some`；[`StaticLabels`] 等
    /// 静态源返回 `None`）。[`crate::Agent::run`] 据此 spawn 周期 `docker version`
    /// 探测；`None` = 不探测（测试静态源避免依赖宿主 docker）。
    fn probe_handle(&self) -> Option<Arc<ContainerProbe>> {
        None
    }
}

/// 容器探测状态（ADR-0018）：周期 `docker version` 探测结果，`sisyphus/container=docker`
/// 标签据此随连接 metadata 上报（探测成功置 true、失败置 false）。`AtomicBool` 经
/// [`LabelSource::labels`] 在每次连接（含重连）即时读取——非阻塞、不在连接路径
/// spawn。周期探测由 [`Self::spawn_refresh`] 后台驱动（首帧即探、之后每
/// [`CONTAINER_PROBE_INTERVAL`]）。
#[derive(Debug)]
pub struct ContainerProbe {
    available: AtomicBool,
}

impl ContainerProbe {
    /// 新建（available=false——首次探测前不置标签）。
    pub fn new() -> Self {
        Self {
            available: AtomicBool::new(false),
        }
    }

    /// 当前 docker 是否可用（探测成功为 true）。
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    /// 置探测结果（[`Self::spawn_refresh`] 内部用；同模块测试用）。
    fn set(&self, v: bool) {
        self.available.store(v, Ordering::Relaxed);
    }

    /// spawn 后台周期探测：首帧即探（首次连接前结果就绪），之后每 `interval`
    /// 重探。返回任务句柄（调用方在退出时 abort）。`docker_bin` 默认
    /// [`crate::container::DOCKER_BIN`]，可注入便于测试。
    pub fn spawn_refresh(
        self: Arc<Self>,
        interval: Duration,
        docker_bin: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.set(probe_once(&docker_bin).await);
                tokio::time::sleep(interval).await;
            }
        })
    }
}

impl Default for ContainerProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// 执行一次 `docker version` 探测：退出 0 = 可用（true）；非零 / spawn 失败 =
/// 不可用（false）。缺 docker 二进制 → spawn 失败 → false（不阻塞启动）。
async fn probe_once(docker_bin: &str) -> bool {
    tokio::process::Command::new(docker_bin)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// 生产标签源：os/arch 静态 + 容器探测动态（[`ContainerProbe`] 最新结果）。
pub struct PlatformLabels {
    container: Arc<ContainerProbe>,
}

impl PlatformLabels {
    /// 以容器探测状态构造（与 [`crate::Agent`] 共享同一 `Arc<ContainerProbe>`——
    /// 后台周期探测更新，连接面 `labels` 即时读取）。
    pub fn new(container: Arc<ContainerProbe>) -> Self {
        Self { container }
    }
}

impl LabelSource for PlatformLabels {
    fn labels(&self) -> Vec<(&'static str, String)> {
        let mut labels = vec![
            (META_OS, os_label().to_string()),
            (META_ARCH, arch_label().to_string()),
        ];
        if self.container.available() {
            labels.push((META_CONTAINER, "docker".to_string()));
        }
        labels
    }

    fn probe_handle(&self) -> Option<Arc<ContainerProbe>> {
        Some(self.container.clone())
    }
}

/// 测试用静态标签源（os/arch，无容器探测）——确定性，不依赖宿主 docker。
/// 集成测试注入此源避免 docker 可用性影响标签断言。
#[derive(Debug, Clone, Default)]
pub struct StaticLabels;

impl LabelSource for StaticLabels {
    fn labels(&self) -> Vec<(&'static str, String)> {
        vec![
            (META_OS, os_label().to_string()),
            (META_ARCH, arch_label().to_string()),
        ]
    }
}

/// 操作系统标签（ADR-0008 取值域：linux/macos/windows）。
fn os_label() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    }
}

/// 架构标签（ADR-0008 取值域：amd64/arm64；std 常量名翻译到调度取值域）。
fn arch_label() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

// ============================================================
// 磁盘占用采样（ADR-0019）
// ============================================================

/// 磁盘采样器：心跳帧附带 [`DiskUsage`] 的来源（缓存/工作区数值随各自批次
/// 换入真实来源；卷级采样本批即真实）。
pub trait DiskSampler: Send + Sync {
    /// 采样当前磁盘占用。
    fn sample(&self) -> DiskUsage;
}

/// 生产实现：卷级 free/total 走平台调用（statvfs / GetDiskFreeSpaceExW），
/// 以数据目录所在卷为采样对象；缓存记账与工作区采样是占位 0（随 cache /
/// workspace 批次换入）。
pub struct PlatformDiskSampler {
    base: PathBuf,
}

impl PlatformDiskSampler {
    /// 以数据目录为采样基准（构建机磁盘 = 数据卷）。
    pub fn new(data_dir: PathBuf) -> Self {
        Self { base: data_dir }
    }
}

impl DiskSampler for PlatformDiskSampler {
    fn sample(&self) -> DiskUsage {
        DiskUsage {
            volumes: platform_volumes(&self.base).unwrap_or_default(),
            cache_bytes: 0,     // 占位：缓存 registry 记账随 cache 批次（ADR-0012）
            workspace_bytes: 0, // 占位：工作区低频采样随 workspace 批次（ADR-0019）
        }
    }
}

/// 卷级磁盘占用（尽力而为：平台调用失败返回 `None`——心跳不带病，缺卷级
/// 数据即空）。
#[cfg(unix)]
#[allow(unsafe_code)] // libc::statvfs 标记 unsafe；FFI 调用，无内部数据访问
fn platform_volumes(base: &std::path::Path) -> Option<Vec<sisyphus_proto::agent::VolumeUsage>> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(base.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // f_frsize 缺失（为 0）的平台回退 f_bsize；free 取 f_bavail（非特权可用）。
    let frsize = if st.f_frsize == 0 {
        st.f_bsize
    } else {
        st.f_frsize
    };
    let total = (st.f_blocks as u64).saturating_mul(frsize as u64);
    let free = (st.f_bavail as u64).saturating_mul(frsize as u64);
    Some(vec![sisyphus_proto::agent::VolumeUsage {
        mount_point: "/".to_string(),
        total_bytes: total.min(i64::MAX as u64) as i64,
        free_bytes: free.min(i64::MAX as u64) as i64,
    }])
}

/// 卷级磁盘占用（Windows：GetDiskFreeSpaceExW 接受任意目录路径）。
/// 单一 FFI 调用的 unsafe（无 unsafe 的数据访问/生命周期）；workspace lint
/// 对 unsafe 是 warn（vendored protoc 先例），此处以显式 allow 标注理由。
#[cfg(windows)]
#[allow(unsafe_code)]
fn platform_volumes(base: &std::path::Path) -> Option<Vec<sisyphus_proto::agent::VolumeUsage>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = base.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    // 第三个参数输出的是卷总字节，第四个是卷剩余字节；free 取「调用者可
    // 用」= 第一个参数（配额感知，语义与 Unix f_bavail 对齐）。
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut free_total) };
    if ok == 0 {
        return None;
    }
    // 挂载点 = 路径所在盘的盘根（如 C:\）——路径规范化为盘根形态。
    let mount_point = drive_root(base).unwrap_or_else(|| base.display().to_string());
    Some(vec![sisyphus_proto::agent::VolumeUsage {
        mount_point,
        total_bytes: total.min(i64::MAX as u64) as i64,
        free_bytes: free_avail.min(i64::MAX as u64) as i64,
    }])
}

/// 其他平台兜底（当前发布矩阵之外）：不采样。
#[cfg(not(any(unix, windows)))]
fn platform_volumes(_base: &std::path::Path) -> Option<Vec<sisyphus_proto::agent::VolumeUsage>> {
    None
}

/// Windows 盘根（`C:\` 形态）：取路径的盘符前缀，规范化成带反斜杠的盘根。
#[cfg(windows)]
fn drive_root(path: &std::path::Path) -> Option<String> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(format!("{}:\\", text[..1].to_ascii_uppercase()))
    } else {
        None
    }
}

// ============================================================
// 指数退避（ADR-0007/0017：1s 起、×2、上限 60s、加抖动、永久重试）
// ============================================================

/// 重连指数退避：序列 `min(base × 2^attempt, cap)`，可选 ±jitter 抖动；
/// 成功常驻后 [`Backoff::reset`] 回到 base 起步（外层裁定）。
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
    jitter: f64,
    rng: u64,
}

/// 退避位移饱和点：`2^30 × base` 远超 60s 上限，仅在 `cap` 缺失（=0）的
/// 畸形配置下兜底防 `1 << attempt` 溢出。
const BACKOFF_SHIFT_SATURATION: u32 = 30;

impl Backoff {
    /// 默认参数（ADR-0007/0017）：1s 起、×2、上限 60s、±20% 抖动。
    pub fn new() -> Self {
        Self::with_params(Duration::from_secs(1), Duration::from_secs(60), 0.2)
    }

    /// 指定参数的退避（测试注入短间隔/零抖动求确定性）。
    pub fn with_params(base: Duration, cap: Duration, jitter: f64) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            jitter,
            rng: 0x9E37_79B9_7F4A_7C15
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(0),
        }
    }

    /// 下一轮等待时长并推进序列。`attempt` 饱和后钉在 `cap`（上限防无限增长）。
    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(BACKOFF_SHIFT_SATURATION);
        let raw = self.base.saturating_mul(1u32 << shift).min(self.cap);
        let jittered = if self.jitter > 0.0 {
            // 抖动幅度 = raw × jitter（±）：同机多 Agent 重连去同步。
            // 有符号偏移在 f64 面算（Duration 乘法不接受负数），再按正负
            // 分加/减落回 Duration。
            let frac = self.next_fraction() * 2.0 - 1.0;
            let delta = raw.as_secs_f64() * self.jitter * frac;
            if delta >= 0.0 {
                raw.saturating_add(Duration::from_secs_f64(delta))
            } else {
                raw.saturating_sub(Duration::from_secs_f64(-delta))
            }
        } else {
            raw
        };
        self.attempt = self.attempt.saturating_add(1);
        jittered
    }

    /// 回到 base 起步（外层在连接常驻一段时间后调用）。
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 微型 xorshift64 随机（无 rand 依赖；0 态已在构造时避开）。
    fn next_fraction(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// 通道配置 / 分派 / 错误
// ============================================================

/// 通道配置（组合根装配；测试可注入短心跳间隔与短退避求确定性）。
#[derive(Clone)]
pub struct ChannelConfig {
    /// Server 地址（gRPC 通道）。
    pub server_url: String,
    /// Agent 长期凭据（缺 = 无凭据连接，被拒后退避重试）。
    pub token: Option<String>,
    /// 心跳间隔（默认 15s，ADR-0007）。
    pub heartbeat_interval: Duration,
    /// 重连退避参数。
    pub backoff: Backoff,
    /// 系统标签源（os/arch + 容器探测）。
    pub labels: Arc<dyn LabelSource>,
    /// 磁盘占用采样器。
    pub disk: Arc<dyn DiskSampler>,
    /// 工作区占用采样间隔（ADR-0011/0019：低频后台遍历，默认 10 分钟；
    /// 测试可注入短间隔避免真实 sleep）。
    pub workspace_sample_interval: Duration,
}

/// 下行分派：单 reader 按消息类型投递到各模块（占位 handle 可收；真实
/// 执行随后续批次换入）。
#[derive(Clone)]
pub struct Dispatch {
    /// JobSpec / Cancel → runner。
    pub runner: mpsc::Sender<ChannelMessage>,
    /// UpgradeCommand → upgrader。
    pub upgrader: mpsc::Sender<ChannelMessage>,
    /// WorkspaceCommand → workspace。
    pub workspace: mpsc::Sender<ChannelMessage>,
    /// CacheCommand → cache。
    pub cache: mpsc::Sender<ChannelMessage>,
}

/// 通道连接错误（外层退避重连的输入；文本原因即可，不承载结构化字段）。
#[derive(Debug)]
pub struct ChannelError(pub String);

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ChannelError {}

impl From<tonic::Status> for ChannelError {
    fn from(status: tonic::Status) -> Self {
        ChannelError(format!("gRPC 调用失败：{status}"))
    }
}

// ============================================================
// 连接生命周期
// ============================================================

/// 建立并维持一次通道连接：握手 → 认证（metadata）→ 版本窗口 → 在途上报
/// → 日志缓冲补传 → 心跳循环 + 下行分派。流结束（对端关流/读失败）即返回，
/// 外层负责退避重连（本函数不含重试逻辑）。
///
/// 每次调用都是一次完整「新握手 + 认证 + 标签刷新」——重连即走同一路径。
pub async fn run_connection(
    cfg: &ChannelConfig,
    dispatch: &Dispatch,
    in_flight: Arc<RwLock<Vec<String>>>,
    logbuf: &LogBuffer,
    runner_uplink: &crate::runner::RunnerUplink,
    workspace: &Workspace,
) -> Result<(), ChannelError> {
    let channel = tonic::transport::Endpoint::from_shared(cfg.server_url.clone())
        .map_err(|e| ChannelError(format!("无效 server-url {}：{e}", cfg.server_url)))?
        .connect()
        .await
        .map_err(|e| ChannelError(format!("连接 {} 失败：{e}", cfg.server_url)))?;
    let mut client = AgentChannelClient::new(channel);

    // 请求流 = 邮箱 → 单 writer → 请求通道。全部上行帧（握手/心跳/在途/
    // ack/状态/日志/列表响应）都经此单 writer 转发，保写序（ADR-0007）。
    let (req_tx, req_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(writer_loop(out_rx, req_tx));

    // 首帧必须是握手（Server 侧语义：认证在收到握手后裁决）。
    let handshake = ChannelMessage {
        kind: Some(Kind::Handshake(Handshake {
            agent_version: Some(agent_version()),
            agent_name: hostname(),
        })),
    };
    out_tx
        .send(handshake)
        .await
        .map_err(|_| ChannelError("上行邮箱关闭".into()))?;

    // 凭据与系统标签走 gRPC metadata（连接面事实；每次连接重建 = 标签刷新，
    // 容器探测随批次换入真实来源后此处自然取得最新值）。
    let mut request = tonic::Request::new(ReceiverStream::new(req_rx));
    if let Some(token) = &cfg.token {
        request.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|e| ChannelError(format!("token 不是合法 metadata：{e}")))?,
        );
    }
    for (name, value) in cfg.labels.labels() {
        request.metadata_mut().insert(
            name,
            MetadataValue::try_from(value)
                .map_err(|e| ChannelError(format!("系统标签 {name} 不是合法 metadata：{e}")))?,
        );
    }

    // 认证拒绝/版本拒连都在 connect 返回 Err（Server 侧 trait 边界）。
    let response = client.connect(request).await?;
    let mut inbound = response.into_inner();

    // 握手回执：Server 版本 → 版本窗口裁决（过新明确报错、拒连重试）。
    let server_version = wait_handshake_reply(&mut inbound).await?;
    match version_window(&server_version, &agent_version()) {
        VersionVerdict::ServerTooNew => {
            let message = format!(
                "Server 版本 {} 过新，Agent 拒连（本地为 {}，ADR-0010/0017：\
                 请升级 Agent 或等待 Server 与本地匹配）",
                fmt_version(&server_version),
                fmt_version(&agent_version()),
            );
            tracing::error!("{message}");
            return Err(ChannelError(message));
        }
        VersionVerdict::Compatible => {
            tracing::info!(
                server_version = %fmt_version(&server_version),
                "握手成功：通道建立（token 认证通过）"
            );
        }
    }

    // 在途任务上报（ADR-0008/0011：重连后 Server 据此重建调度状态；首连
    // 时空集亦上报，路径一致）。
    let job_ids = in_flight.read().await.clone();
    out_tx
        .send(ChannelMessage {
            kind: Some(Kind::JobReported(JobReported {
                job_ids: job_ids.clone(),
            })),
        })
        .await
        .map_err(|_| ChannelError("上行邮箱关闭".into()))?;

    // 日志缓冲补传（ADR-0007/0013）：先注入活体发送器（连接期新增日志经活体
    // 转发），再幂等重放——从每个缓冲文件头重发未清空段；重复段由 Server 按
    // seq 幂等吸收。孤儿缓冲（job 不在在途集）重放后删除——执行丢弃、日志
    // 保留作取证后再清（ADR-0013）。
    //
    // 顺序说明：set_live 先于 replay——运行中 job（#59 起）的连接期新增日志
    // 须立即活体转发（重放不截断缓冲，活体 seq 恒高于已落盘段）。同
    // (job, attempt) 内重放读与追加写由 logbuf 的 open 锁互斥（无撕裂读）；
    // 追加活体转发与重放发送的到达交错由 Server 按 seq 幂等落库吸收——最终
    // 状态恒正确，符合「不做 per-batch ack 等待」的补传语义（ADR-0013）。
    logbuf.set_live(Some(out_tx.clone())).await;
    // 工作区列表响应上行（ADR-0011）：与 logbuf 同款活体注入——连接期内
    // workspace Handle 的列表响应经此单 writer 外送，断线置 None（断线不重发
    // 列表查询，UI 可重发）。
    workspace.set_live(Some(out_tx.clone())).await;
    // runner 上行链路（ADR-0008/0013）：JobAck/JobStatus 活体发送 + 离线终态
    // 缓冲。set_live 先于 flush_pending——离线期间完成的终态经此 sender 补发
    // （< orphan 宽限窗口；超宽限由 Server orphan grace 兜底）。
    runner_uplink.set_live(Some(out_tx.clone())).await;
    for msg in logbuf
        .replay_all()
        .await
        .map_err(|e| ChannelError(format!("日志缓冲重放失败：{e}")))?
    {
        out_tx
            .send(msg)
            .await
            .map_err(|_| ChannelError("上行邮箱关闭".into()))?;
    }
    for (job_id, attempt) in logbuf.orphans(&job_ids) {
        logbuf.clear_now(&job_id, attempt);
    }
    // 离线期间缓冲的终态补发（日志重放之后；同一 writer 保写序）。
    runner_uplink.flush_pending(&out_tx).await;

    // 心跳循环：15s 一报，附带磁盘占用（ADR-0019）。独立 task——连接期内
    // 与 reader 并行；流结束由外层 abort。工作区占用从 [`Workspace`] 的低频
    // 采样取最近值（卷级 + 缓存来自 disk 采样器，工作区来自 workspace 采样）。
    let heartbeat_tx = out_tx.clone();
    let heartbeat_interval = cfg.heartbeat_interval;
    let disk = cfg.disk.clone();
    let workspace_hb = workspace.clone();
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(heartbeat_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let mut usage = disk.sample();
            usage.workspace_bytes = workspace_hb.workspace_bytes();
            let msg = ChannelMessage {
                kind: Some(Kind::Heartbeat(Heartbeat { disk: Some(usage) })),
            };
            if heartbeat_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 单 reader 循环：下行帧按类型分派到各模块；流读完（对端关流）或读
    // 失败即结束连接（外层退避重连）。清除活体日志转发——此后事件仅落缓冲，
    // 重连时重放补传。
    let result = read_and_dispatch(&mut inbound, dispatch).await;
    logbuf.set_live(None).await;
    workspace.set_live(None).await;
    runner_uplink.set_live(None).await;
    heartbeat.abort();
    writer.abort();
    result
}

/// 读握手回执：循环读到首帧 Handshake 即返回其 Agent 版本（Server 回发
/// 自身版本）。读流错误/提前关流按连接失败处理。
async fn wait_handshake_reply(
    inbound: &mut tonic::Streaming<ChannelMessage>,
) -> Result<Version, ChannelError> {
    while let Some(msg) = inbound
        .message()
        .await
        .map_err(|e| ChannelError(format!("读握手回包失败：{e}")))?
    {
        if let Some(Kind::Handshake(h)) = msg.kind {
            return h
                .agent_version
                .ok_or_else(|| ChannelError("Server 握手回包缺版本".into()));
        }
    }
    Err(ChannelError("Server 未回发握手（流提前关闭）".into()))
}

/// 单 reader 下行分派循环：JobSpec/Cancel → runner，Upgrade → upgrader，
/// WorkspaceCommand → workspace，CacheCommand → cache；冗余握手与契约未知
/// 变体（未来加字段）忽略。流读完返回 `Ok(())`（对端关流），读失败返回
/// `Err`。
async fn read_and_dispatch(
    inbound: &mut tonic::Streaming<ChannelMessage>,
    dispatch: &Dispatch,
) -> Result<(), ChannelError> {
    while let Some(msg) = inbound
        .message()
        .await
        .map_err(|e| ChannelError(format!("读下行帧失败：{e}")))?
    {
        match msg.kind {
            Some(Kind::JobSpec(_)) | Some(Kind::Cancel(_)) => {
                dispatch_to(&dispatch.runner, "runner", msg).await;
            }
            Some(Kind::Upgrade(_)) => {
                dispatch_to(&dispatch.upgrader, "upgrader", msg).await;
            }
            Some(Kind::WorkspaceCmd(_)) => {
                dispatch_to(&dispatch.workspace, "workspace", msg).await;
            }
            Some(Kind::CacheCmd(_)) => {
                dispatch_to(&dispatch.cache, "cache", msg).await;
            }
            // 冗余握手（重连竞态回发）与未知变体：忽略（契约演进只加字段）。
            _ => {}
        }
    }
    Ok(())
}

/// 向某模块通道投递一帧下行消息（模块 handle 未启动/已退出 = 通道关闭，
/// 记警告不 panic——reader 继续服务其余模块）。
async fn dispatch_to(channel: &mpsc::Sender<ChannelMessage>, module: &str, msg: ChannelMessage) {
    if channel.send(msg).await.is_err() {
        tracing::warn!("{module} 分派通道已关闭，下行帧丢弃");
    }
}

/// 单 writer：把上行邮箱里的帧按序转发进请求流（保写序，ADR-0007）。
async fn writer_loop(
    mut out_rx: mpsc::Receiver<ChannelMessage>,
    req_tx: mpsc::Sender<ChannelMessage>,
) {
    while let Some(msg) = out_rx.recv().await {
        if req_tx.send(msg).await.is_err() {
            break; // 请求流已关闭（对端断开）：writer 结束
        }
    }
}

// ============================================================
// 单元测试（纯逻辑：版本窗口/标签/退避；磁盘采样器真调用平台 API）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn version_window_rejects_server_newer_and_accepts_rest() {
        let local = v(1, 0, 0);
        assert_eq!(
            version_window(&v(1, 0, 0), &local),
            VersionVerdict::Compatible
        );
        assert_eq!(
            version_window(&v(0, 9, 0), &local),
            VersionVerdict::Compatible,
            "旧 Server 可连"
        );
        assert_eq!(
            version_window(&v(1, 1, 0), &local),
            VersionVerdict::ServerTooNew
        );
        assert_eq!(
            version_window(&v(2, 0, 0), &local),
            VersionVerdict::ServerTooNew
        );
    }

    #[test]
    fn platform_labels_reflect_container_probe() {
        // probe=false（默认）→ os/arch 上报、不置容器标签。
        let probe = Arc::new(ContainerProbe::new());
        let labels = PlatformLabels::new(probe).labels();
        let names: Vec<_> = labels.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&META_OS), "os 标签应上报");
        assert!(names.contains(&META_ARCH), "arch 标签应上报");
        assert!(!names.contains(&META_CONTAINER), "probe=false 不置容器标签");
        let os = labels
            .iter()
            .find(|(n, _)| *n == META_OS)
            .expect("os")
            .1
            .clone();
        assert!(matches!(os.as_str(), "linux" | "macos" | "windows"));
        let arch = labels
            .iter()
            .find(|(n, _)| *n == META_ARCH)
            .expect("arch")
            .1
            .clone();
        assert!(
            matches!(arch.as_str(), "amd64" | "arm64"),
            "arch 应落在调度取值域"
        );

        // probe=true → sisyphus/container=docker 随 labels 上报。
        let probe = Arc::new(ContainerProbe::new());
        probe.set(true);
        let labels = PlatformLabels::new(probe).labels();
        assert!(
            labels
                .iter()
                .any(|(n, v)| *n == META_CONTAINER && v == "docker"),
            "probe=true → sisyphus/container=docker"
        );

        // probe_handle：PlatformLabels 返回 Some（供 Agent::run spawn 周期探测）。
        let probe = Arc::new(ContainerProbe::new());
        let labels = PlatformLabels::new(probe.clone());
        assert!(
            labels.probe_handle().is_some(),
            "PlatformLabels 有 probe_handle"
        );
        // StaticLabels：os/arch、无容器、无 probe_handle（确定性，不依赖宿主 docker）。
        let labels = StaticLabels.labels();
        let names: Vec<_> = labels.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&META_OS));
        assert!(names.contains(&META_ARCH));
        assert!(
            !names.contains(&META_CONTAINER),
            "StaticLabels 不置容器标签"
        );
        assert!(
            StaticLabels.probe_handle().is_none(),
            "StaticLabels 无 probe_handle"
        );
    }

    /// probe_once：缺二进制 → spawn 失败 → false（不阻塞、确定性，无需 daemon）。
    #[tokio::test]
    async fn probe_once_missing_binary_returns_false() {
        assert!(
            !probe_once("sisyphus-no-such-docker-zzz").await,
            "缺 docker 二进制 → false"
        );
    }

    #[test]
    fn backoff_escalates_exponentially_and_caps() {
        let mut b = Backoff::with_params(Duration::from_secs(1), Duration::from_secs(60), 0.0);
        let expected = [1, 2, 4, 8, 16, 32, 60, 60, 60];
        for secs in expected {
            assert_eq!(b.next_delay(), Duration::from_secs(secs), "退避序列");
        }
        // 复位回 base 起步。
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_jitter_stays_within_band() {
        let mut b = Backoff::with_params(Duration::from_secs(10), Duration::from_secs(60), 0.2);
        // 每段退避都应在该段指数值 ±20% 带内（含上限段：60 ± 12）。
        for _ in 0..20 {
            let d = b.next_delay().as_secs_f64();
            let nominal = (10.0 * 2f64.powi((b.attempt - 1) as i32)).min(60.0);
            assert!(
                (nominal * 0.8 - 1e-6..=nominal * 1.2 + 1e-6).contains(&d),
                "退避段 {nominal}s 的抖动应在 ±20% 带内：{d}"
            );
        }
    }

    #[test]
    fn platform_disk_sampler_reports_positive_volume() {
        let dir = tempfile::tempdir().expect("临时目录");
        let sampler = PlatformDiskSampler::new(dir.path().to_path_buf());
        let disk = sampler.sample();
        // 卷级采样真实可用（平台调用成功则 total>0）；缓存/工作区是占位 0。
        assert_eq!(disk.cache_bytes, 0);
        assert_eq!(disk.workspace_bytes, 0);
        if let Some(volume) = disk.volumes.first() {
            assert!(volume.total_bytes > 0, "卷总量应为正");
            assert!(volume.free_bytes >= 0 && volume.free_bytes <= volume.total_bytes);
        }
        // 平台调用失败（如 sandbox 缺权限）也允许空——心跳不带病。
    }
}

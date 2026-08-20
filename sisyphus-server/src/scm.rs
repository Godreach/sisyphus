//! scm 模块：SCM 集成层 trait 缝 + 真实探测（ADR-0016，票 B2c-T6 / B5-T3）。
//!
//! poll 触发源探测经本 trait 缝隔离：trigger 模块消费 [`ScmProbe::probe_head`]
//! 拿到项目默认分支 / HEAD 的当前提交标识（git head sha / svn HEAD
//! revision），用以基线 / 去重 / 触发。真实 `git ls-remote` / `svn info`
//! 探测见 [`SystemScmProbe`]（B5-T3 落地：shell 出系统 git/svn 客户端、
//! 凭据经 ASKPASS/递送、缺二进制清晰报错、错误不回显凭据）；测试面注入
//! [`FakeProbe`]（可控 head / 失败序列）验证基线 / 节奏 / 去重 / 历史逻辑。
//!
//! 服务端凭据递送（ASKPASS 助手脚本 Unix / credential store 文件 Windows /
//! svn `--password-from-stdin`）镜像 Agent 侧 `checkout.rs` 的递送形态
//! （ADR-0016「复用 Agent 侧 checkout 的递送形态」）：server 不依赖 agent
//! crate（agent 为叶子二进制），故此处按同形态实现一份服务端子集（探测为
//! 一次性 capture，无 streaming/多命令序列）；未来可抽共享 crate 收敛。
//!
//! cron 触发源不经探测（默认值 + 默认分支 head：触发上下文钉默认分支、
//! 不钉 commit，Agent 执行期检分支头），故本 trait 仅服务 poll。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;

use crate::secrets::MasterKey;
use crate::store::projects::Project;
use crate::store::scm_credentials::ScmCredentialRepo;

/// SCM 探测端口（trait 缝，ADR-0016）：poll 触发源经此拿项目默认分支 /
/// HEAD 的当前提交标识。生产面是真实 git / svn 探测（随 scm 批次），测试
/// 面注入 [`FakeProbe`]。
///
/// `#[tonic::async_trait]` 使 trait 对象可用（`Arc<dyn ScmProbe>`，与 sched
/// 的 [`crate::sched::JobDispatcher`] 同款 dyn-compatible async trait）。
#[tonic::async_trait]
pub trait ScmProbe: Send + Sync {
    /// 探测项目默认分支 / HEAD 的当前最新提交标识。
    ///
    /// - `Ok(Some(id))`：git 为 head commit sha、svn 为 HEAD revision 字符串
    ///   ——trigger 模块按 `project.scm_type` 映射到 [`crate::engine::TriggerDetail`]
    ///   的 `commit`（git）或 `revision`（svn）。
    /// - `Ok(None)`：空仓库（无提交）——不触发，记探测成功（清历史错误）。
    /// - `Err(msg)`：探测失败——记入触发器历史 `last_probe_error`、按节奏
    ///   重试、不自动禁用（ADR-0016）。
    async fn probe_head(&self, project: &Project) -> Result<Option<String>, String>;
}

/// 占位探测（生产面，随 scm 批次替换为真实 git / svn 探测）：一律返回探测
/// 失败，错误信息标注「尚未实现」。poll 触发源据此记 `last_probe_error`、
/// 按节奏重试、不自动禁用——cron 触发源不经探测，照常工作。真实探测落地
/// 后在 main 装配处换入即可。
#[derive(Debug, Default, Clone)]
pub struct UnimplementedProbe;

#[tonic::async_trait]
impl ScmProbe for UnimplementedProbe {
    async fn probe_head(&self, _project: &Project) -> Result<Option<String>, String> {
        Err("SCM 探测尚未实现（随 scm 批次落地）".into())
    }
}

/// 可控探测（测试 / 无网络面，票 B2c-T6 假探测）：按编程结果序列逐次返回
/// [`probe_head`](ScmProbe::probe_head) 的结果。每次调用弹出队首结果；队空
/// 兜底 `Ok(None)`（空仓库）——测试应编程到预期点再用 [`Self::pending`]
/// 断言「已消费尽」，避免误落兜底。
///
/// 序列形态契合 poll 的「逐次探测」节奏：一次 poll tick 一次 `probe_head`，
/// 测试按时间轴压入「基线 → 新提交 → 失败 → 再成功」等序列驱动基线 / 去重
/// / 失败历史断言（假时钟配合，不依赖真实 sleep）。
#[derive(Debug, Default)]
pub struct FakeProbe {
    results: Mutex<VecDeque<Outcome>>,
}

/// 单次探测的编程结果（FakeProbe 内部）。
#[derive(Debug, Clone)]
enum Outcome {
    /// 探测成功：`Some(id)` 有提交 / `None` 空仓库。
    Head(Option<String>),
    /// 探测失败（错误信息）。
    Error(String),
}

impl FakeProbe {
    /// 新建空序列探测（队空兜底 `Ok(None)`）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 压入一次探测成功（`Some(id)` 有提交 / `None` 空仓库）。
    pub fn push_head(&self, head: Option<&str>) {
        let mut g = self.results.lock().expect("FakeProbe 锁");
        g.push_back(Outcome::Head(head.map(str::to_string)));
    }

    /// 压入一次探测失败（错误信息）。
    pub fn push_error(&self, message: &str) {
        let mut g = self.results.lock().expect("FakeProbe 锁");
        g.push_back(Outcome::Error(message.into()));
    }

    /// 剩余编程结果数（断言「已消费到预期点」用）。
    pub fn pending(&self) -> usize {
        self.results.lock().expect("FakeProbe 锁").len()
    }
}

#[tonic::async_trait]
impl ScmProbe for FakeProbe {
    async fn probe_head(&self, _project: &Project) -> Result<Option<String>, String> {
        let outcome = self.results.lock().expect("FakeProbe 锁").pop_front();
        match outcome {
            Some(Outcome::Head(h)) => Ok(h),
            Some(Outcome::Error(e)) => Err(e),
            None => Ok(None),
        }
    }
}

// ============================================================
// 真实探测（B5-T3，ADR-0016）：shell 出系统 git/svn + 凭据递送
// ============================================================

/// SCM 二进制名/路径。默认 `git`/`svn`（PATH 查找）；测试注入不存在路径以验
/// 「缺二进制清晰报错」（ADR-0016）。将来 Server 配置可覆写为绝对路径。
#[derive(Debug, Clone)]
pub struct ScmBins {
    /// git 二进制（默认 `"git"`）。
    pub git: String,
    /// svn 二进制（默认 `"svn"`）。
    pub svn: String,
}

impl Default for ScmBins {
    fn default() -> Self {
        Self {
            git: "git".into(),
            svn: "svn".into(),
        }
    }
}

/// 解密后的 SCM 凭据（探测用；明文仅在内存，探测毕即弃，永不上命令行/URL，
/// ADR-0015/0016）。由 [`SystemScmProbe`] 从库密文解密、或 API 层从请求体
/// 直接构造（ad-hoc 探测，不落库）。
#[derive(Debug, Clone)]
pub(crate) struct PlainScmCred {
    /// 用户名（非机密；svn `--username` 进 args、git ASKPASS 读 env）。
    pub(crate) username: String,
    /// 密码/token（机密；git 经 ASKPASS/credential store、svn 经 stdin 递送）。
    pub(crate) password: String,
}

impl PlainScmCred {
    /// 由明文用户名 + 密码构造（ad-hoc 探测用）。
    pub(crate) fn new(username: String, password: String) -> Self {
        Self { username, password }
    }
}

/// 探测错误（可读消息；**永不含凭据**——username/password 绝不进消息，
/// 原始 stderr 也不外泄，ADR-0016「凭据错误不回显凭据」）。失败原因按退出码
/// /stderr 关键词归类为有限可读消息，其余只报退出码。
#[derive(Debug, Clone)]
pub(crate) enum ProbeError {
    /// 缺 git/svn 二进制（前置要求 git ≥ 2.20 / svn ≥ 1.10；清晰报错不静默降级）。
    MissingBinary(&'static str),
    /// 认证 / 权限失败（凭据错或无权限；提示检查凭据，不回显凭据）。
    AuthFailed,
    /// 仓库不存在 / 不可达（URL 错或远端无此仓库）。
    RepoNotFound,
    /// 其它失败（提示文本已脱敏，不含凭据/原始 stderr；退出码已并入提示）。
    Other { hint: String },
}

impl ProbeError {
    /// 人读消息（trait 缝的 `Err(String)` 与 API 层错误文案的共同来源）。
    pub(crate) fn message(&self) -> String {
        match self {
            Self::MissingBinary(name) => {
                format!("探测失败：缺 {name} 二进制（前置要求 git ≥ 2.20 / svn ≥ 1.10）")
            }
            Self::AuthFailed => "认证失败：凭据或权限不足，请检查 SCM 凭据与仓库访问权限".into(),
            Self::RepoNotFound => "仓库不存在或不可达，请检查仓库 URL".into(),
            Self::Other { hint } => format!("探测失败：{hint}"),
        }
    }
}

/// 把非零退出的 git/svn 输出归类为 [`ProbeError`]（按 stderr 关键词；不外泄
/// stderr 原文，仅用于内部分类）。`bin` 仅供 Other 提示，不含凭据。
fn classify_failure(bin: &str, code: i32, stderr: &[u8]) -> ProbeError {
    let stderr = String::from_utf8_lossy(stderr).to_lowercase();
    // 认证/权限类（git/svn 跨工具常见措辞）。
    let auth = [
        "authentication failed",
        "access denied",
        "authorization",
        "permission denied",
        "401",
        "403",
        "could not read username",
        "no credentials",
    ];
    let not_found = [
        "not found",
        "404",
        "does not appear to be a git repository",
        "no such repository",
        "could not read from remote repository",
        "unable to connect to a repository",
    ];
    if auth.iter().any(|k| stderr.contains(k)) {
        return ProbeError::AuthFailed;
    }
    if not_found.iter().any(|k| stderr.contains(k)) {
        return ProbeError::RepoNotFound;
    }
    ProbeError::Other {
        hint: format!("{bin} 退出码 {code}"),
    }
}

// ============================================================
// 服务端凭据递送（镜像 Agent 侧 checkout.rs 的递送形态，ADR-0016）
// ============================================================
//
// 与 agent checkout.rs 同款：git 走 GIT_ASKPASS 助手脚本（Unix，静态内容读
// env、不含密码）+ SISY_SCM_USER/PASS env；Windows ASKPASS 兼容性不佳 → 回退
// 临时 credential store 文件（0600 等价、探测毕即删）。svn 走 `--username` +
// `--password-from-stdin`（密码经 stdin）。密码绝不在 args/URL（ADR-0015）。
//
// 服务端探测为一次性 capture（tokio::process::Command + output），无 streaming；
// artifact 临时文件名按进程内原子计数器唯一化，免并发探测互踩。

/// GIT_ASKPASS 助手脚本内容（静态、不含密码）：读 `$SISY_SCM_USER`/
/// `$SISY_SCM_PASS`，按 prompt 回显用户名或密码。`#!/bin/sh` + 0700 使 git
/// 可直接 invoke（与 agent checkout 同款）。
#[cfg(any(unix, test))]
fn askpass_helper_script() -> &'static str {
    "#!/bin/sh\ncase \"$1\" in\n*Username*) echo \"$SISY_SCM_USER\" ;;\n*) echo \"$SISY_SCM_PASS\" ;;\nesac\n"
}

/// git credential store 文件内容（Windows 回退，ADR-0016）：一行
/// `https://<user>:<pass>@<host>`（与 agent checkout 同款）。
#[cfg(any(windows, test))]
fn credstore_content(user: &str, pass: &str, host: &str) -> String {
    format!("https://{user}:{pass}@{host}\n")
}

/// 从 `https://host/path[.git]` URL 抽 host；非 https（ssh/file）→ None（与
/// agent checkout 同款：ssh 走密钥、file:// 无需认证）。
#[cfg(any(windows, test))]
fn https_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or("");
    let host = host.split('@').next_back().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// git 凭据递送 env（注入 git 命令）：
/// - Unix：`GIT_ASKPASS=<path>` + `SISY_SCM_USER/PASS`（ASKPASS 助手脚本读）+
///   `GIT_TERMINAL_PROMPT=0`（不向终端要凭据，只走 ASKPASS）。
/// - Windows：`GIT_CONFIG_COUNT/KEY_0/VALUE_0` 指向 credential store 文件
///   （密码在文件、不在 env/args）。
///
/// 无凭据 / 非 https（Windows）→ 空 env（与 agent checkout 同款）。
#[cfg(unix)]
fn git_cred_env_for(
    cred: &PlainScmCred,
    artifact: Option<&std::path::Path>,
) -> Vec<(String, String)> {
    let Some(path) = artifact else {
        return Vec::new();
    };
    let path = path.to_string_lossy().into_owned();
    vec![
        ("GIT_ASKPASS".into(), path),
        ("SISY_SCM_USER".into(), cred.username.clone()),
        ("SISY_SCM_PASS".into(), cred.password.clone()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]
}

#[cfg(windows)]
fn git_cred_env_for(
    _cred: &PlainScmCred,
    artifact: Option<&std::path::Path>,
) -> Vec<(String, String)> {
    let Some(path) = artifact else {
        return Vec::new();
    };
    let path = path.to_string_lossy().into_owned();
    vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), format!("store --file={path}")),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]
}

#[cfg(not(any(unix, windows)))]
fn git_cred_env_for(
    _cred: &PlainScmCred,
    _artifact: Option<&std::path::Path>,
) -> Vec<(String, String)> {
    Vec::new()
}

/// 写凭据递送 artifact（Unix：ASKPASS 助手脚本 0700 / Windows：credential
/// store 文件，0600 等价——经 icacls 移除继承 ACE 仅留当前用户，ADR-0016）。
#[cfg(unix)]
fn write_cred_artifact(
    path: &std::path::Path,
    _cred: &PlainScmCred,
    _repo_url: &str,
) -> std::io::Result<()> {
    std::fs::write(path, askpass_helper_script())?;
    set_mode(path, 0o700)
}

#[cfg(windows)]
fn write_cred_artifact(
    path: &std::path::Path,
    cred: &PlainScmCred,
    repo_url: &str,
) -> std::io::Result<()> {
    let Some(host) = https_host(repo_url) else {
        return std::fs::write(path, b"");
    };
    std::fs::write(
        path,
        credstore_content(&cred.username, &cred.password, &host),
    )?;
    if let Err(e) = restrict_acl_owner_only(path) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_cred_artifact(
    _path: &std::path::Path,
    _cred: &PlainScmCred,
    _repo_url: &str,
) -> std::io::Result<()> {
    Ok(())
}

/// Windows：`icacls <file> /inheritance:r /grant:r <USERNAME>:F`（0600 等价）。
#[cfg(windows)]
fn restrict_acl_owner_only(path: &std::path::Path) -> std::io::Result<()> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return Err(std::io::Error::other(
            "无法确定当前用户名（USERNAME 环境变量空）",
        ));
    }
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "icacls 限制 ACL 失败（exit {}）",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
}

/// Unix chmod；其它平台 no-op（与 agent checkout 同款）。
#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// 凭据 artifact 临时路径：系统 temp + `sisyphus-scm-probe-<id>` 后缀（Unix 加
/// `.sh`）。id 按进程内原子计数器唯一化，免并发探测互踩（agent checkout 用
/// job_id 唯一化，服务端探测无 job_id 故用计数器）。
fn cred_artifact_path(id: u64) -> PathBuf {
    let name = if cfg!(unix) {
        format!("sisyphus-scm-probe-{id}.sh")
    } else {
        format!("sisyphus-scm-probe-{id}")
    };
    std::env::temp_dir().join(name)
}

/// 进程内唯一 id 序列（凭据 artifact 命名用）。
static PROBE_ARTIFACT_ID: AtomicU64 = AtomicU64::new(0);

fn next_probe_id() -> u64 {
    PROBE_ARTIFACT_ID.fetch_add(1, Ordering::Relaxed)
}

/// 凭据递送 artifact 生命周期：写盘 + Drop 清理（探测毕即删，ADR-0016）。
struct GitCredDelivery {
    artifact: Option<PathBuf>,
}

impl GitCredDelivery {
    /// 准备 git 凭据递送。无凭据（None）或 username+password 皆空 → 无 artifact
    /// （空 delivery，env 空）。写盘失败 → Err（探测失败，detail 写明）。
    fn prepare(cred: Option<&PlainScmCred>, repo_url: &str) -> std::io::Result<Self> {
        let Some(c) = cred else {
            return Ok(Self { artifact: None });
        };
        if c.username.is_empty() && c.password.is_empty() {
            return Ok(Self { artifact: None });
        }
        let path = cred_artifact_path(next_probe_id());
        if let Err(e) = write_cred_artifact(&path, c, repo_url) {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        Ok(Self {
            artifact: Some(path),
        })
    }

    /// 凭据递送 env（注入 git 命令）。无 artifact → 空。
    fn env(&self, cred: Option<&PlainScmCred>) -> Vec<(String, String)> {
        match cred {
            Some(c) => git_cred_env_for(c, self.artifact.as_deref()),
            None => Vec::new(),
        }
    }
}

impl Drop for GitCredDelivery {
    fn drop(&mut self) {
        if let Some(p) = self.artifact.take() {
            let _ = std::fs::remove_file(&p);
        }
    }
}

// ============================================================
// 子命令执行（tokio 一次性 capture；缺二进制清晰报错）
// ============================================================

/// 起一个 git 命令（带凭据递送 env），capture 全部输出。缺 git 二进制 →
/// [`ProbeError::MissingBinary`]（不静默降级，ADR-0016）。
async fn run_git(
    args: &[&str],
    repo_url: &str,
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<std::process::Output, ProbeError> {
    let delivery = GitCredDelivery::prepare(cred, repo_url).map_err(|e| ProbeError::Other {
        hint: format!("凭据递送准备失败：{e}"),
    })?;
    let env = delivery.env(cred);
    let mut cmd = Command::new(&bins.git);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ProbeError::MissingBinary("git"),
        _ => ProbeError::Other {
            hint: format!("启动 git 失败：{e}"),
        },
    })
}

/// 起一个 svn 命令（凭据经 `--username` + `--password-from-stdin` + stdin）。
/// 缺 svn 二进制 → [`ProbeError::MissingBinary`]。
async fn run_svn(
    args: &[&str],
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<std::process::Output, ProbeError> {
    let mut args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let stdin = match cred {
        Some(c) if !c.username.is_empty() && !c.password.is_empty() => {
            // 用户名 + 密码：`--username` 进 args（非机密）、`--password-from-stdin`
            // 经 stdin 递送密码（绝不上命令行，ADR-0015/0016）。
            args.push("--username".into());
            args.push(c.username.clone());
            args.push("--password-from-stdin".into());
            Some(c.password.clone().into_bytes())
        }
        // 仅密码（无用户名）或仅用户名：svn `--password-from-stdin` 必须配
        // `--username`，无法单独送密码 → 不接管 stdin（与 git ASKPASS 仍送密码
        // 的行为不同，但 svn 无「仅密码」协议形态，不静默吞凭据：仍以匿名
        // 探测，多半落到 AuthFailed，而非静默成功）。
        _ => None,
    };
    let mut cmd = Command::new(&bins.svn);
    cmd.args(args.iter().map(|s| s.as_str()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = if let Some(password) = stdin {
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProbeError::MissingBinary("svn"),
            _ => ProbeError::Other {
                hint: format!("启动 svn 失败：{e}"),
            },
        })?;
        // `--password-from-stdin` 读一行：写 <password> + 换行，再关 stdin。
        use tokio::io::AsyncWriteExt;
        if let Some(mut child_stdin) = child.stdin.take() {
            let _ = child_stdin.write_all(&password).await;
            let _ = child_stdin.write_all(b"\n").await;
            let _ = child_stdin.shutdown().await;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| ProbeError::Other {
                hint: format!("svn 等待失败：{e}"),
            })?
    } else {
        cmd.stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ProbeError::MissingBinary("svn"),
                _ => ProbeError::Other {
                    hint: format!("启动 svn 失败：{e}"),
                },
            })?
    };
    Ok(output)
}

// ============================================================
// 探测原语（git ls-remote / svn info）—— poll 与 ad-hoc 端点共用
// ============================================================

/// `git ls-remote --symref <url> HEAD` → (HEAD sha, 默认分支)。
///
/// - 成功 + 有输出：sha = `HEAD` 指向的提交；默认分支 = symref 行
///   `ref: refs/heads/<b>`（detached HEAD → None）。
/// - 成功 + 空输出：空仓库 → (None, None)。
/// - 非零退出：按 stderr 归类 [`ProbeError`]（不外泄 stderr/凭据）。
pub(crate) async fn git_ls_remote_head(
    url: &str,
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<(Option<String>, Option<String>), ProbeError> {
    let output = run_git(&["ls-remote", "--symref", url, "HEAD"], url, cred, bins).await?;
    if !output.status.success() {
        return Err(classify_failure(
            "git",
            output.status.code().unwrap_or(-1),
            &output.stderr,
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ls_remote_symref(&stdout))
}

/// 解析 `git ls-remote --symref <url> HEAD` 输出 → (HEAD sha, 默认分支)。
/// 形态：可选 `ref: refs/heads/<b>\tHEAD` 行 + `<sha>\tHEAD` 行。空 → (None, None)。
fn parse_ls_remote_symref(stdout: &str) -> (Option<String>, Option<String>) {
    let mut sha = None;
    let mut default_branch = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ref: ") {
            // `ref: refs/heads/main\tHEAD` → 默认分支 main。
            if let Some(b) = rest
                .split_whitespace()
                .next()
                .and_then(|r| r.strip_prefix("refs/heads/"))
            {
                default_branch = Some(b.to_string());
            }
        } else if let Some((s, refname)) = line.split_once('\t')
            && refname.trim() == "HEAD"
            && s.chars().all(|c| c.is_ascii_hexdigit())
        {
            sha = Some(s.to_string());
        }
    }
    (sha, default_branch)
}

/// `git ls-remote --symref <url>` → (分支清单, 默认分支)。单次往返即同取分支
/// 列表（`refs/heads/*` 行）与默认分支（`--symref` 解析的 `ref: refs/heads/<b>`），
/// 免分支枚举端点对私有仓库发两次凭据递送（ADR-0016）。detached HEAD →
/// 默认分支 None；分支清单只含 `refs/heads/*`。
///
/// 不用 `--symref --heads`：`--heads` 过滤掉 `HEAD`、连带丢 `--symref` 解析的
/// symref 行（`ref: refs/heads/<b>\tHEAD`）——默认分支就解析不出了。裸
/// `--symref` 既出 symref 行又出全部分支，单次即足。
pub(crate) async fn git_ls_remote_branches(
    url: &str,
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<(Vec<(String, String)>, Option<String>), ProbeError> {
    let output = run_git(&["ls-remote", "--symref", url], url, cred, bins).await?;
    if !output.status.success() {
        return Err(classify_failure(
            "git",
            output.status.code().unwrap_or(-1),
            &output.stderr,
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok((
        parse_ls_remote_heads(&stdout),
        parse_default_branch(&stdout),
    ))
}

/// 解析 `git ls-remote --symref ...` 输出里的 `ref: refs/heads/<b>` 行 → 默认分支。
/// detached HEAD（无 symref 行）→ None。
fn parse_default_branch(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("ref: ")
            && let Some(b) = rest
                .split_whitespace()
                .next()
                .and_then(|r| r.strip_prefix("refs/heads/"))
        {
            return Some(b.to_string());
        }
    }
    None
}

/// 解析 `git ls-remote --heads <url>` 输出 → Vec<(分支, sha)>。每行
/// `<sha>\trefs/heads/<branch>`。
fn parse_ls_remote_heads(stdout: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some((sha, refname)) = line.split_once('\t')
            && let Some(branch) = refname.strip_prefix("refs/heads/")
        {
            out.push((branch.to_string(), sha.to_string()));
        }
    }
    out
}

/// `svn info --show-item revision <url>` → Option<revision>。空输出 → None。
pub(crate) async fn svn_info_revision(
    url: &str,
    cred: Option<&PlainScmCred>,
    bins: &ScmBins,
) -> Result<Option<String>, ProbeError> {
    let output = run_svn(
        &["info", "--show-item", "revision", url, "--non-interactive"],
        cred,
        bins,
    )
    .await?;
    if !output.status.success() {
        return Err(classify_failure(
            "svn",
            output.status.code().unwrap_or(-1),
            &output.stderr,
        ));
    }
    let rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if rev.is_empty() { None } else { Some(rev) })
}

// ============================================================
// SystemScmProbe：生产探测（poll 用，解密存储凭据）
// ============================================================

/// 生产 SCM 探测（B5-T3）：解密项目存储凭据 → shell 出 `git ls-remote` /
/// `svn info` 探测 head。装配于 main（替换 [`UnimplementedProbe`]）。凭据经
/// ADR-0015 链路解密（[`crate::secrets::decrypt`] + 主密钥），探测毕即弃，
/// 永不上命令行/URL。缺 git/svn 二进制 → 清晰报错（不静默降级，ADR-0016）。
pub struct SystemScmProbe {
    repo: ScmCredentialRepo,
    master_key: MasterKey,
    bins: ScmBins,
}

impl SystemScmProbe {
    /// 装配：凭据 repo（解密存储凭据）+ 主密钥 + SCM 二进制（默认 PATH 查找）。
    pub fn new(repo: ScmCredentialRepo, master_key: MasterKey, bins: ScmBins) -> Self {
        Self {
            repo,
            master_key,
            bins,
        }
    }

    /// 解密项目存储凭据 → `Option<PlainScmCred>`（无行 / username+password 皆空
    /// → None：公开 / file:// 仓库免认证）。解密失败 → Err（探测失败）。
    async fn load_cred(&self, project: &Project) -> Result<Option<PlainScmCred>, String> {
        let row = self
            .repo
            .get(project.id)
            .await
            .map_err(|e| format!("读取 SCM 凭据失败：{e}"))?;
        resolve_plain_cred(row, &self.master_key)
    }
}

/// 解密一条 SCM 凭据行 → `Option<PlainScmCred>`（无行 / username+password 皆空
/// → None：公开 / file:// 仓库免认证）。解密失败 → Err（调用侧映射为探测失败）。
/// poll 的 [`SystemScmProbe`] 与既有项目的测试连接端点共用此解密逻辑。
pub(crate) fn resolve_plain_cred(
    row: Option<crate::store::scm_credentials::ScmCredentialRow>,
    master_key: &MasterKey,
) -> Result<Option<PlainScmCred>, String> {
    let Some(row) = row else {
        return Ok(None);
    };
    let username = row.username.unwrap_or_default();
    let password = match row.password_ciphertext {
        Some(blob) => {
            let plain = crate::secrets::decrypt(master_key, &blob)
                .map_err(|e| format!("SCM 凭据解密失败：{e}"))?;
            String::from_utf8_lossy(&plain).into_owned()
        }
        None => String::new(),
    };
    if username.is_empty() && password.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlainScmCred::new(username, password)))
}

#[tonic::async_trait]
impl ScmProbe for SystemScmProbe {
    async fn probe_head(&self, project: &Project) -> Result<Option<String>, String> {
        let cred = self.load_cred(project).await?;
        match project.scm_type {
            crate::store::projects::ScmType::Git => {
                git_ls_remote_head(&project.scm_url, cred.as_ref(), &self.bins)
                    .await
                    .map(|(sha, _)| sha)
                    .map_err(|e| e.message())
            }
            crate::store::projects::ScmType::Svn => {
                svn_info_revision(&project.scm_url, cred.as_ref(), &self.bins)
                    .await
                    .map_err(|e| e.message())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::projects::ScmType;

    fn proj() -> Project {
        Project {
            id: 1,
            name: "demo".into(),
            scm_type: ScmType::Git,
            scm_url: "https://example.com/repo".into(),
            default_branch: Some("main".into()),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn fake_probe_replays_programmed_sequence() {
        let probe = FakeProbe::new();
        probe.push_head(Some("abc"));
        probe.push_head(None);
        probe.push_error("git ls-remote failed");
        // 队列按入队顺序弹出。
        assert_eq!(
            probe.probe_head(&proj()).await.expect("成功").as_deref(),
            Some("abc")
        );
        assert_eq!(probe.probe_head(&proj()).await.expect("空仓库"), None);
        assert_eq!(
            probe.probe_head(&proj()).await.unwrap_err(),
            "git ls-remote failed"
        );
        assert_eq!(probe.pending(), 0, "已消费尽");
    }

    #[tokio::test]
    async fn fake_probe_empty_queue_falls_back_to_none() {
        let probe = FakeProbe::new();
        assert_eq!(probe.probe_head(&proj()).await.expect("兜底"), None);
    }

    #[tokio::test]
    async fn unimplemented_probe_always_errors() {
        let probe = UnimplementedProbe;
        assert!(probe.probe_head(&proj()).await.is_err());
    }

    // ---- 解析原语（纯）----

    #[test]
    fn parse_symref_extracts_head_sha_and_default_branch() {
        let out = "ref: refs/heads/main\tHEAD\nabc123def0000000000000000000000000000\tHEAD\n";
        let (sha, br) = parse_ls_remote_symref(out);
        assert_eq!(
            sha.as_deref(),
            Some("abc123def0000000000000000000000000000")
        );
        assert_eq!(br.as_deref(), Some("main"));
    }

    #[test]
    fn parse_symref_detached_head_has_no_default_branch() {
        // 无 symref 行（detached HEAD）→ 只有 sha，默认分支 None。
        let out = "abc123def0000000000000000000000000000\tHEAD\n";
        let (sha, br) = parse_ls_remote_symref(out);
        assert_eq!(
            sha.as_deref(),
            Some("abc123def0000000000000000000000000000")
        );
        assert_eq!(br, None);
    }

    #[test]
    fn parse_symref_empty_repo_returns_none_none() {
        assert_eq!(parse_ls_remote_symref(""), (None, None));
        assert_eq!(parse_ls_remote_symref("\n\n"), (None, None));
    }

    #[test]
    fn parse_heads_lists_branches() {
        let out = "aaa\trefs/heads/main\nbbb\trefs/heads/dev\n";
        let branches = parse_ls_remote_heads(out);
        assert_eq!(
            branches,
            vec![("main".into(), "aaa".into()), ("dev".into(), "bbb".into()),]
        );
        assert!(parse_ls_remote_heads("").is_empty());
    }

    #[test]
    fn askpass_helper_script_has_no_credentials() {
        let script = askpass_helper_script();
        assert!(script.starts_with("#!/bin/sh"), "shebang 使可 invoke");
        assert!(script.contains("SISY_SCM_USER"), "读 env 取用户名");
        assert!(script.contains("SISY_SCM_PASS"), "读 env 取密码");
        // 脚本静态、不含任何具体凭据字面量。
        assert!(!script.contains("password="));
        assert!(!script.contains("secret"));
    }

    #[test]
    fn credstore_content_formats_user_pass_host() {
        assert_eq!(
            credstore_content("alice", "p@ss", "github.com"),
            "https://alice:p@ss@github.com\n"
        );
    }

    #[test]
    fn https_host_extracts_from_https_url() {
        assert_eq!(
            https_host("https://github.com/org/repo.git"),
            Some("github.com".into())
        );
        assert_eq!(
            https_host("https://gitlab.com:443/path"),
            Some("gitlab.com:443".into())
        );
        assert_eq!(
            https_host("https://user@host.com/x"),
            Some("host.com".into())
        );
        assert_eq!(
            https_host("git@github.com:org/repo.git"),
            None,
            "ssh 非 https"
        );
        assert_eq!(https_host("file:///path/to/repo"), None, "file 非 https");
    }

    #[test]
    fn parse_default_branch_from_symref_line() {
        // --symref --heads 输出含 symref 行 + 分支行。
        let out = "ref: refs/heads/main\tHEAD\nabc\trefs/heads/main\ndef\trefs/heads/dev\n";
        assert_eq!(parse_default_branch(out), Some("main".into()));
        // 无 symref 行（detached HEAD）→ None。
        assert_eq!(parse_default_branch("abc\trefs/heads/main\n"), None);
        assert_eq!(parse_default_branch(""), None);
    }

    #[tokio::test]
    async fn svn_probe_missing_binary_is_clear_error() {
        let bins = ScmBins {
            git: "git".into(),
            svn: "sisyphus-no-such-svn-zzz".into(),
        };
        let err = svn_info_revision("https://example.com/svn", None, &bins)
            .await
            .expect_err("缺 svn 应报错");
        assert!(
            matches!(err, ProbeError::MissingBinary("svn")),
            "缺 svn：{err:?}"
        );
        assert!(err.message().contains("缺") && err.message().contains("svn"));
    }

    #[test]
    fn classify_failure_maps_known_stderr_keywords() {
        assert!(matches!(
            classify_failure("git", 128, b"fatal: Authentication failed"),
            ProbeError::AuthFailed
        ));
        assert!(matches!(
            classify_failure("git", 128, b"fatal: could not read from remote repository"),
            ProbeError::RepoNotFound
        ));
        // 未知 stderr → Other（只报退出码，不外泄 stderr）。
        match classify_failure("svn", 1, b"some weird error") {
            ProbeError::Other { hint } => {
                assert!(
                    hint.contains("svn") && hint.contains('1'),
                    "hint 含 bin + 退出码：{hint}"
                );
                assert!(!hint.contains("weird"), "不外泄 stderr：{hint}");
            }
            other => panic!("应 Other：{other:?}"),
        }
    }

    // ---- 真实 git 探测（本地裸仓库 fixture）----

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command as StdCommand;

    /// 创建本地裸仓库（main + dev 两分支、main 一个提交），返回裸仓库路径 +
    /// main 的 HEAD sha。`git ls-remote <bare>` 可读其 refs。
    fn make_bare_git_repo(parent: &Path, name: &str) -> (PathBuf, String) {
        let src = parent.join(format!("{name}-src"));
        fs::create_dir_all(&src).expect("建 src 目录");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(&src)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?} 失败：{}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };
        git(&["init", "--quiet"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "test@sisyphus.local"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::write(src.join("hello.txt"), "v1\n").expect("写文件");
        git(&["add", "hello.txt"]);
        git(&["commit", "--quiet", "-m", "v1"]);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("utf8")
            .trim()
            .to_string();
        git(&["branch", "dev"]); // 第二分支。
        let bare = parent.join(format!("{name}-bare"));
        StdCommand::new("git")
            .args([
                "clone",
                "--bare",
                "--quiet",
                &src.to_string_lossy(),
                &bare.to_string_lossy(),
            ])
            .output()
            .expect("clone --bare");
        (bare, sha)
    }

    #[tokio::test]
    async fn git_ls_remote_head_returns_sha_and_default_branch() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (bare, sha) = make_bare_git_repo(dir.path(), "repo");
        let bins = ScmBins::default();
        let (head, default_branch) = git_ls_remote_head(&bare.to_string_lossy(), None, &bins)
            .await
            .expect("探测应成功");
        assert_eq!(head.as_deref(), Some(sha.as_str()), "HEAD sha 匹配");
        assert_eq!(default_branch.as_deref(), Some("main"), "默认分支 = main");
    }

    #[tokio::test]
    async fn git_ls_remote_branches_lists_all_branches_and_default() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (bare, sha) = make_bare_git_repo(dir.path(), "repo");
        let bins = ScmBins::default();
        let (branches, default_branch) =
            git_ls_remote_branches(&bare.to_string_lossy(), None, &bins)
                .await
                .expect("探测应成功");
        let names: Vec<&str> = branches.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"main"), "含 main：{names:?}");
        assert!(names.contains(&"dev"), "含 dev：{names:?}");
        let main = branches.iter().find(|(n, _)| n == "main").unwrap();
        assert_eq!(main.1, sha, "main 的 sha 匹配");
        assert_eq!(default_branch.as_deref(), Some("main"), "默认分支 main");
    }

    #[tokio::test]
    async fn git_probe_missing_binary_is_clear_error() {
        let bins = ScmBins {
            git: "sisyphus-no-such-git-zzz".into(),
            svn: "svn".into(),
        };
        let err = git_ls_remote_head("https://example.com/repo", None, &bins)
            .await
            .expect_err("缺 git 应报错");
        assert!(
            matches!(err, ProbeError::MissingBinary("git")),
            "缺 git：{err:?}"
        );
        assert!(err.message().contains("缺") && err.message().contains("git"));
    }

    #[tokio::test]
    async fn git_probe_bad_url_error_does_not_echo_credentials() {
        let dir = tempfile::tempdir().expect("临时目录");
        let bins = ScmBins::default();
        // 不存在的本地路径 → 仓库不可达；带凭据（含已知密码）探测。
        let cred = PlainScmCred::new("alice".into(), "hunter2-secret-pw".into());
        let bad_url = dir
            .path()
            .join("no-such-repo")
            .to_string_lossy()
            .into_owned();
        let err = git_ls_remote_head(&bad_url, Some(&cred), &bins)
            .await
            .expect_err("坏 URL 应报错");
        let msg = err.message();
        assert!(
            !msg.contains("hunter2-secret-pw"),
            "错误消息不得回显密码：{msg}"
        );
        assert!(!msg.contains("alice"), "不回显用户名：{msg}");
    }

    // ---- SystemScmProbe（解密存储凭据 → 探测）----

    use crate::config;
    use crate::secrets::encrypt;
    use crate::store::projects::{NewProject, ProjectRepo};
    use crate::store::scm_credentials::ScmCredentialRepo;
    use sqlx::SqlitePool;

    /// 已迁移库 + 空白项目（scm_url 后填），返回 (pool, master_key, project_id)。
    async fn probe_fixture(scm_url: String) -> (SqlitePool, MasterKey, i64) {
        let dir = tempfile::tempdir().expect("临时目录");
        config::Config::load(
            dir.path().to_path_buf(),
            config::Overrides::default(),
            config::Overrides::default(),
        )
        .expect("目录布局");
        let pool = crate::store::bootstrap(dir.path())
            .await
            .expect("bootstrap");
        let project = ProjectRepo::new(pool.clone())
            .create(NewProject {
                name: "demo".into(),
                scm_type: ScmType::Git,
                scm_url,
                default_branch: None,
            })
            .await
            .expect("建项目");
        // TempDir 活到测试结束：存进静态槽。
        PROBE_LEAK.lock().expect("leak").push(dir);
        (pool, MasterKey::generate(), project.id)
    }

    static PROBE_LEAK: std::sync::Mutex<Vec<tempfile::TempDir>> = std::sync::Mutex::new(Vec::new());

    async fn make_project_with_id(pool: &SqlitePool, id: i64) -> Project {
        ProjectRepo::new(pool.clone())
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn system_probe_returns_head_for_public_local_repo() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (bare, sha) = make_bare_git_repo(dir.path(), "repo");
        let (pool, key, project_id) = probe_fixture(bare.to_string_lossy().into_owned()).await;
        let project = make_project_with_id(&pool, project_id).await;
        let probe = SystemScmProbe::new(ScmCredentialRepo::new(pool), key, ScmBins::default());
        let head = probe.probe_head(&project).await.expect("探测应成功");
        assert_eq!(head.as_deref(), Some(sha.as_str()));
        let _ = dir; // bare 在 dir 下，但 dir 独立于 fixture 的 temp dir。
    }

    #[tokio::test]
    async fn system_probe_with_stored_credential_still_works_on_local_repo() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (bare, sha) = make_bare_git_repo(dir.path(), "repo");
        let (pool, key, project_id) = probe_fixture(bare.to_string_lossy().into_owned()).await;
        // 存一份凭据（加密）；本地裸仓库免认证，凭据递送不破坏探测。
        let blob = encrypt(&key, b"some-token").expect("加密");
        ScmCredentialRepo::new(pool.clone())
            .set(project_id, Some("alice"), Some(&blob), "admin", 0)
            .await
            .expect("存凭据");
        let project = make_project_with_id(&pool, project_id).await;
        let probe = SystemScmProbe::new(ScmCredentialRepo::new(pool), key, ScmBins::default());
        let head = probe.probe_head(&project).await.expect("探测应成功");
        assert_eq!(head.as_deref(), Some(sha.as_str()));
        let _ = dir;
    }

    #[tokio::test]
    async fn system_probe_missing_binary_is_clear_error_message() {
        let (pool, key, project_id) = probe_fixture("https://example.com/repo".into()).await;
        let project = make_project_with_id(&pool, project_id).await;
        let probe = SystemScmProbe::new(
            ScmCredentialRepo::new(pool),
            key,
            ScmBins {
                git: "sisyphus-no-such-git-zzz".into(),
                svn: "svn".into(),
            },
        );
        let err = probe.probe_head(&project).await.expect_err("缺 git 应报错");
        assert!(err.contains("缺") && err.contains("git"), "清晰报错：{err}");
    }
}

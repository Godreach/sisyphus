//! checkout 执行器（ADR-0016/0011；票 B3-T6 / #60）。
//!
//! Agent 侧 checkout scm 步骤的命令编排 + 凭据递送 + 脱敏链路。runner（#59）
//! 遇 `StepKind::Checkout` 即调 [`run`]；step start/end 事件由 runner 包裹，本
//! 模块只产出子命令的输出流（经 [`crate::stepio`] 流式编码 + 脱敏 + 截断）与
//! 终态 [`JobOutcome`]。
//!
//! - **shell 出系统 git/svn 客户端**（ADR-0016）：不内嵌 git 库；缺二进制清晰
//!   报错（[`crate::exec::spawn_command`] 的 `NotFound` → 「缺 X 二进制」），
//!   不静默降级。
//! - **git 增量**（ADR-0011/0016）：首次 `clone`（全量，不浅克隆）；复用工作区
//!   `fetch origin <分支>` → `checkout --detach <target>` → `reset --hard
//!   <target>` → `clean -fd`（无 `-x`，保 `.git` 与忽略文件）。`target` = commit
//!   sha（钉到确切提交）；commit 空 → `origin/<分支>`（分支头，ADR-0016「Agent
//!   检分支头」）。
//! - **svn 增量**：首次 `checkout [-r <rev>]`；复用 `cleanup` + `update -r <rev>`
//!   （rev 空 → HEAD）。
//! - **子模块**（git）：`submodules == true` → `submodule update --init --recursive`；
//!   `false` → 跳过。「默认开」由 Server 解析后下发，Agent 按解析后的 bool 执行
//!   （不在 Agent 侧二次默认——覆盖用户显式关闭反成 bug）。
//! - **凭据递送**（ADR-0015/0016，永不上命令行/URL）：git 走 `GIT_ASKPASS`
//!   助手脚本（Unix，静态内容读 env、不含密码）+ `SISY_SCM_USER/PASS` env；
//!   Windows ASKPASS 兼容性不佳 → 回退临时 credential store 文件（0600、任务
//!   毕即删）。svn 走 `--username` + `--password-from-stdin`（密码经 stdin）。
//!   密码进 [`crate::redact`] 脱敏集（runner `collect_secrets` 已收 checkout
//!   凭据 password），输出离机前字面量替换。
//! - **取消/超时**：逐子命令与 job 级 deadline/cancel 竞争（经
//!   [`crate::stepio::run_streamed_step`]）；命中即终止余下命令、返回终态。
//! - **容器任务挂载 ASKPASS 的约束**：本票定形（凭据递送只经 env/stdin/临时
//!   文件，不进 args/URL）；容器内执行随 #53。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sisyphus_proto::agent::{CheckoutStep, ScmCredential, VcsType};
use tokio::sync::watch;

use crate::exec::SpawnError;
use crate::logbuf::LogBuffer;
use crate::runner::JobOutcome;
use crate::stepio::{self, Truncation};

// ============================================================
// SCM 二进制名/路径（默认 + 可注入测缺二进制）
// ============================================================

/// SCM 二进制名/路径。默认 `git`/`svn`（PATH 查找）；测试注入不存在路径以验
/// 「缺二进制清晰报错」（ADR-0016）。将来 Agent 配置可覆写为绝对路径。
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

// ============================================================
// 已规划的子命令（argv 直传，凭据绝不在 args）
// ============================================================

/// 一条已规划的 checkout 子命令：`program` + `args`（argv 直传，凭据绝不在
/// `args`）+ `cred_env`（git ASKPASS/credential store env）+ `stdin`（svn
/// `--password-from-stdin` 的密码）+ `cwd`。`label` 供失败报错点名。
#[derive(Debug, Clone)]
pub(crate) struct PlannedCommand {
    program: String,
    args: Vec<String>,
    cred_env: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
    cwd: PathBuf,
    label: &'static str,
}

// ============================================================
// step start 命令回显（脱敏摘要：repo_url + 目标，绝不含凭据）
// ============================================================

/// `CheckoutStep.vcs`（裸 i32）→ `Option<VcsType>`。prost 枚举字段是裸 i32、
/// 越界值（如 999）不报错；本助手显式判越界以清晰报错（ADR-0016「缺/未知不
/// 静默降级」）。
pub(crate) fn vcs_of(step: &CheckoutStep) -> Option<VcsType> {
    match step.vcs {
        x if x == VcsType::VcsGit as i32 => Some(VcsType::VcsGit),
        x if x == VcsType::VcsSvn as i32 => Some(VcsType::VcsSvn),
        _ => None,
    }
}

/// checkout 步骤 start 事件的命令回显：`<vcs> checkout <repo_url> @ <target>`，
/// 不含凭据（repo_url 是项目 URL、对 runner 可见；commit/rev 是提交标识）。
pub(crate) fn step_echo(step: &CheckoutStep) -> String {
    match vcs_of(step) {
        Some(VcsType::VcsSvn) => {
            let rev = if step.r#ref.is_empty() {
                "HEAD".into()
            } else {
                step.r#ref.clone()
            };
            format!("svn checkout {} @ r{}", step.repo_url, rev)
        }
        _ => {
            let target = if !step.commit.is_empty() {
                step.commit.clone()
            } else if !step.r#ref.is_empty() {
                step.r#ref.clone()
            } else {
                "?".into()
            };
            format!("git checkout {} @ {}", step.repo_url, target)
        }
    }
}

// ============================================================
// 规划（纯：不执行、不写盘）
// ============================================================

/// 规划 checkout 子命令序列（纯）。凭据经 `cred_env`（git）/ `credential`
/// （svn `--username` + stdin）注入——密码绝不在 `args`（ADR-0015/0016）。
/// `need_init = true` → 首次（git clone / svn checkout）；`false` → 增量
/// （git fetch / svn cleanup+update）。规划错（缺 repo_url / 未知 vcs / git
/// 无 commit 无 branch）→ `Err(detail)`，调用方映射到 `SpawnFailed`。
pub(crate) fn plan(
    step: &CheckoutStep,
    ws_dir: &Path,
    need_init: bool,
    bins: &ScmBins,
    cred_env: &[(String, String)],
    credential: Option<&ScmCredential>,
) -> Result<Vec<PlannedCommand>, String> {
    match vcs_of(step) {
        Some(VcsType::VcsGit) => plan_git(step, ws_dir, need_init, bins, cred_env),
        Some(VcsType::VcsSvn) => plan_svn(step, ws_dir, need_init, bins, credential),
        None => Err("未知 VcsType（CheckoutStep.vcs 越界）".into()),
    }
}

/// git 子命令序列。`cred_env` 注入每条 git 命令（ASKPASS/credential store env）。
fn plan_git(
    step: &CheckoutStep,
    ws_dir: &Path,
    need_init: bool,
    bins: &ScmBins,
    cred_env: &[(String, String)],
) -> Result<Vec<PlannedCommand>, String> {
    if step.repo_url.is_empty() {
        return Err("CheckoutStep 缺 repo_url".into());
    }
    let branch = step.r#ref.as_str();
    let commit = step.commit.as_str();
    let mut cmds = Vec::new();
    if need_init {
        // 首次全量克隆（ADR-0016：不浅克隆；工作区持久摊销一次性成本）。
        cmds.push(PlannedCommand {
            program: bins.git.clone(),
            args: vec![
                "clone".into(),
                step.repo_url.clone(),
                ws_dir.to_string_lossy().into_owned(),
            ],
            cred_env: cred_env.to_vec(),
            stdin: None,
            // clone 目标 = ws_dir（绝对）；cwd 取父目录即可（target 已绝对）。
            cwd: ws_dir.parent().unwrap_or(ws_dir).to_path_buf(),
            label: "git clone",
        });
    } else {
        // 增量：fetch origin <branch>（branch 空 → fetch origin 全量）。
        let mut fetch_args = vec!["fetch".into(), "origin".into()];
        if !branch.is_empty() {
            fetch_args.push(branch.to_string());
        }
        cmds.push(PlannedCommand {
            program: bins.git.clone(),
            args: fetch_args,
            cred_env: cred_env.to_vec(),
            stdin: None,
            cwd: ws_dir.to_path_buf(),
            label: "git fetch",
        });
    }
    // 钉到确切提交：commit 有 → 钉 commit；commit 空 → 分支头 origin/<branch>
    // （ADR-0016「Agent 检分支头」）；两者皆空 → 无法钉点，报错。
    let target = if !commit.is_empty() {
        commit.to_string()
    } else if !branch.is_empty() {
        format!("origin/{branch}")
    } else {
        return Err("git checkout 缺 commit 与 branch（无法钉到目标）".into());
    };
    cmds.push(PlannedCommand {
        program: bins.git.clone(),
        args: vec!["checkout".into(), "--detach".into(), target.clone()],
        cred_env: cred_env.to_vec(),
        stdin: None,
        cwd: ws_dir.to_path_buf(),
        label: "git checkout --detach",
    });
    cmds.push(PlannedCommand {
        program: bins.git.clone(),
        args: vec!["reset".into(), "--hard".into(), target],
        cred_env: cred_env.to_vec(),
        stdin: None,
        cwd: ws_dir.to_path_buf(),
        label: "git reset --hard",
    });
    // clean -fd（无 -x：保忽略文件，ADR-0011）。
    cmds.push(PlannedCommand {
        program: bins.git.clone(),
        args: vec!["clean".into(), "-fd".into()],
        cred_env: cred_env.to_vec(),
        stdin: None,
        cwd: ws_dir.to_path_buf(),
        label: "git clean -fd",
    });
    // 子模块开关（默认开由 Server 解析；Agent 按解析后 bool 执行，ADR-0016）。
    if step.submodules {
        cmds.push(PlannedCommand {
            program: bins.git.clone(),
            args: vec![
                "submodule".into(),
                "update".into(),
                "--init".into(),
                "--recursive".into(),
            ],
            cred_env: cred_env.to_vec(),
            stdin: None,
            cwd: ws_dir.to_path_buf(),
            label: "git submodule update --init --recursive",
        });
    }
    Ok(cmds)
}

/// svn 子命令序列。凭据经 `--username <user> --password-from-stdin` + stdin
/// （密码绝不在 args）。无凭据 → 不带认证参数（公开仓库 / file://）。
fn plan_svn(
    step: &CheckoutStep,
    ws_dir: &Path,
    need_init: bool,
    bins: &ScmBins,
    credential: Option<&ScmCredential>,
) -> Result<Vec<PlannedCommand>, String> {
    if step.repo_url.is_empty() {
        return Err("CheckoutStep 缺 repo_url".into());
    }
    let rev = step.r#ref.as_str();
    let mut cmds = Vec::new();
    if need_init {
        // 首次 checkout [-r <rev>]（rev 空 → HEAD）。
        let mut args = vec!["checkout".into()];
        if !rev.is_empty() {
            args.push("-r".into());
            args.push(rev.to_string());
        }
        args.push(step.repo_url.clone());
        args.push(ws_dir.to_string_lossy().into_owned());
        push_svn_auth(&mut args, credential);
        cmds.push(PlannedCommand {
            program: bins.svn.clone(),
            args,
            cred_env: Vec::new(),
            stdin: svn_stdin(credential),
            cwd: ws_dir.parent().unwrap_or(ws_dir).to_path_buf(),
            label: "svn checkout",
        });
    } else {
        // 增量：cleanup（本地 WC 修复，无需凭据）+ update [-r <rev>]。
        cmds.push(PlannedCommand {
            program: bins.svn.clone(),
            args: vec!["cleanup".into(), ws_dir.to_string_lossy().into_owned()],
            cred_env: Vec::new(),
            stdin: None,
            cwd: ws_dir.to_path_buf(),
            label: "svn cleanup",
        });
        let mut up_args = vec!["update".into()];
        if !rev.is_empty() {
            up_args.push("-r".into());
            up_args.push(rev.to_string());
        }
        up_args.push(ws_dir.to_string_lossy().into_owned());
        push_svn_auth(&mut up_args, credential);
        cmds.push(PlannedCommand {
            program: bins.svn.clone(),
            args: up_args,
            cred_env: Vec::new(),
            stdin: svn_stdin(credential),
            cwd: ws_dir.to_path_buf(),
            label: "svn update",
        });
    }
    Ok(cmds)
}

/// svn 认证参数：有凭据 → `--username <user> --password-from-stdin`（密码经
/// stdin，绝不上命令行；ADR-0015/0016）。无凭据 → 不加（svn 不提示认证）。
fn push_svn_auth(args: &mut Vec<String>, credential: Option<&ScmCredential>) {
    if let Some(c) = credential
        && !c.username.is_empty()
    {
        args.push("--username".into());
        args.push(c.username.clone());
        args.push("--password-from-stdin".into());
    }
}

/// svn 密码 stdin 数据：有凭据且 username 非空 → `<password>\n`（svn
/// `--password-from-stdin` 读一行）。否则 None（不接管 stdin）。
fn svn_stdin(credential: Option<&ScmCredential>) -> Option<Vec<u8>> {
    let c = credential?;
    if c.username.is_empty() {
        return None;
    }
    Some(format!("{}\n", c.password).into_bytes())
}

// ============================================================
// git 凭据递送（Unix ASKPASS 助手脚本 / Windows 临时 credential store 文件）
// ============================================================

/// GIT_ASKPASS 助手脚本内容（静态、不含密码）：读 `$SISY_SCM_USER`/
/// `$SISY_SCM_PASS`（由 git 命令 env 注入、helper 继承），按 prompt 回显用户名
/// 或密码。`#!/bin/sh` shebang + 0700 使 git 可直接 invoke。
///
/// 跨平台纯函数（仅返回脚本字符串，不执行），故 `cfg(any(unix, test))`：Unix
/// lib 构建由 [`write_cred_artifact`] 用、其它平台仅测试用。
#[cfg(any(unix, test))]
fn askpass_helper_script() -> &'static str {
    "#!/bin/sh\ncase \"$1\" in\n*Username*) echo \"$SISY_SCM_USER\" ;;\n*) echo \"$SISY_SCM_PASS\" ;;\nesac\n"
}

/// git credential store 文件内容（Windows 回退，ADR-0016）：一行
/// `https://<user>:<pass>@<host>`。`store --file=<path>` 读此文件递凭据。
///
/// 跨平台纯函数，`cfg(any(windows, test))`：Windows lib 构建由
/// [`write_cred_artifact`] 用、其它平台仅测试用。
#[cfg(any(windows, test))]
fn credstore_content(user: &str, pass: &str, host: &str) -> String {
    format!("https://{user}:{pass}@{host}\n")
}

/// 从 `https://host/path[.git]` URL 抽 host；非 https（ssh/file）→ None（不适用
/// credential store：ssh 走密钥、file:// 无需认证）。
///
/// 跨平台纯函数，`cfg(any(windows, test))`：Windows lib 构建由
/// [`write_cred_artifact`] 用、其它平台仅测试用。
#[cfg(any(windows, test))]
fn https_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or("");
    let host = host.split('@').next_back().unwrap_or(host); // 容忍 user@host 写法
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// git 凭据递送 env（注入每条 git 命令）：
/// - Unix：`GIT_ASKPASS=<path>` + `SISY_SCM_USER/PASS`（helper 读）+
///   `GIT_TERMINAL_PROMPT=0`（不向终端要凭据，只走 ASKPASS）。
/// - Windows：`GIT_CONFIG_COUNT/KEY_0/VALUE_0` 指向 credential store 文件
///   （密码在文件、不在 env/args）。
///
/// 无凭据 / 非 https（Windows）→ 空 env（git 走免认证或既有登录态）。
#[cfg(unix)]
fn git_cred_env(cred: Option<&ScmCredential>, artifact: Option<&Path>) -> Vec<(String, String)> {
    let (Some(c), Some(path)) = (cred, artifact) else {
        return Vec::new();
    };
    let path = path.to_string_lossy().into_owned();
    vec![
        ("GIT_ASKPASS".into(), path),
        ("SISY_SCM_USER".into(), c.username.clone()),
        ("SISY_SCM_PASS".into(), c.password.clone()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]
}

#[cfg(windows)]
fn git_cred_env(cred: Option<&ScmCredential>, artifact: Option<&Path>) -> Vec<(String, String)> {
    let (Some(_c), Some(path)) = (cred, artifact) else {
        return Vec::new();
    };
    let path = path.to_string_lossy().into_owned();
    vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), format!("store --file={path}")),
        // 与 Unix 同：禁止向终端要凭据（credential store 失败时也不弹
        // Git-Credential-Manager 对话框挂起非交互 Agent）。
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]
}

#[cfg(not(any(unix, windows)))]
fn git_cred_env(_cred: Option<&ScmCredential>, _artifact: Option<&Path>) -> Vec<(String, String)> {
    Vec::new()
}

/// 写凭据递送 artifact（Unix：ASKPASS 助手脚本 0700 / Windows：credential
/// store 文件，0600 等价——经 `icacls` 移除继承 ACE 仅留当前用户，ADR-0016）。
/// 无凭据 → 调用方不调本函数。写盘失败 → Err（checkout 失败，detail 写明）。
#[cfg(unix)]
fn write_cred_artifact(path: &Path, _cred: &ScmCredential, _repo_url: &str) -> std::io::Result<()> {
    std::fs::write(path, askpass_helper_script())?;
    // 0700：git 直接 invoke 助手脚本需可执行。
    set_mode(path, 0o700)
}

#[cfg(windows)]
fn write_cred_artifact(path: &Path, cred: &ScmCredential, repo_url: &str) -> std::io::Result<()> {
    let Some(host) = https_host(repo_url) else {
        // 非 https（ssh/file）：credential store 不适用，写一个空文件占位
        // （env 仍指向它，但 git 不会从中取 https 凭据；ssh 走密钥、file 无需认证）。
        return std::fs::write(path, b"");
    };
    std::fs::write(
        path,
        credstore_content(&cred.username, &cred.password, &host),
    )?;
    // 0600 等价：移除继承 ACE、仅当前用户 Full（ADR-0016「0600」的 Windows 等价）。
    // icacls 是 Windows 核心工具，与 git/svn 同款 shellout 纪律（不内嵌 unsafe FFI）。
    // 限制失败即删文件 + 报错（fail-closed：不留无保护的明文密码文件）。
    if let Err(e) = restrict_acl_owner_only(path) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// Windows：`icacls <file> /inheritance:r /grant:r <USERNAME>:F` —— 移除继承
/// ACE、仅当前用户 Full 控制权限（0600 等价，ADR-0016）。icacls 退出非零 → Err。
#[cfg(windows)]
fn restrict_acl_owner_only(path: &Path) -> std::io::Result<()> {
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

#[cfg(not(any(unix, windows)))]
fn write_cred_artifact(
    _path: &Path,
    _cred: &ScmCredential,
    _repo_url: &str,
) -> std::io::Result<()> {
    Ok(())
}

/// Unix chmod（0700）；其它平台 no-op。
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// 凭据递送 artifact 生命周期：写盘 + Drop 清理（任务毕即删，ADR-0016）。
struct GitCredDelivery {
    artifact: Option<PathBuf>,
}

impl GitCredDelivery {
    /// 准备 git 凭据递送。无凭据 / username+password 皆空 → 无 artifact（空
    /// delivery，env 空）。写盘失败 → Err。
    fn prepare(
        credential: Option<&ScmCredential>,
        job_id: &str,
        repo_url: &str,
    ) -> std::io::Result<Self> {
        let Some(c) = credential else {
            return Ok(Self { artifact: None });
        };
        if c.username.is_empty() && c.password.is_empty() {
            return Ok(Self { artifact: None });
        }
        let path = cred_artifact_path(job_id);
        if let Err(e) = write_cred_artifact(&path, c, repo_url) {
            // 写失败清理半截文件，避免泄漏空文件。
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        Ok(Self {
            artifact: Some(path),
        })
    }

    /// 凭据递送 env（注入每条 git 命令）。无 artifact → 空。
    fn env(&self, cred: Option<&ScmCredential>) -> Vec<(String, String)> {
        git_cred_env(cred, self.artifact.as_deref())
    }
}

impl Drop for GitCredDelivery {
    fn drop(&mut self) {
        if let Some(p) = self.artifact.take() {
            // 任务毕即删（尽力而为：不存在/删失败忽略——临时文件，不复用）。
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// 凭据 artifact 临时路径：系统 temp + `sisyphus-scm-<job_id>` 后缀（Unix 加
/// `.sh` 助手脚本后缀）。同 job 去重（runner 已保证同 job 不并发），跨 job 唯一。
fn cred_artifact_path(job_id: &str) -> PathBuf {
    let name = if cfg!(unix) {
        format!("sisyphus-scm-{job_id}.sh")
    } else {
        format!("sisyphus-scm-{job_id}")
    };
    std::env::temp_dir().join(name)
}

// ============================================================
// 子命令执行器（host / 容器共用缝：只差「进程怎么起」，ADR-0018）
// ============================================================

/// checkout 子命令执行器：把 (program, args, cwd, env, pipe_stdin) 起成一个
/// [`crate::exec::SpawnedStep`]（+ 可选容器名，供取消/超时补刀 `docker rm -f`）。
/// host 后端 argv 直传、返回 `None` 名；容器后端包成 `docker run`、返回 `Some(name)`
/// （见 [`crate::container`]）。[`run_planned`] 经此 trait 与后端解耦——两后端共用
/// 同一套子命令执行循环，只换 spawner。
pub(crate) trait CommandSpawner: Send + Sync {
    /// 起一个子命令进程，返回 (进程句柄, 可选容器名)。缺二进制（host）→
    /// [`crate::exec::SpawnError`] 携「缺 X 二进制」清晰报错，不静默降级（ADR-0016）。
    fn spawn(
        &self,
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
        pipe_stdin: bool,
    ) -> Result<(crate::exec::SpawnedStep, Option<String>), SpawnError>;
}

/// 宿主机直跑执行器：argv 直传 [`crate::exec::spawn_command`]；无容器名（`None`）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HostSpawner;

impl CommandSpawner for HostSpawner {
    fn spawn(
        &self,
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
        pipe_stdin: bool,
    ) -> Result<(crate::exec::SpawnedStep, Option<String>), SpawnError> {
        let spawned = crate::exec::spawn_command(program, args, cwd, env, pipe_stdin)?;
        Ok((spawned, None))
    }
}

/// 执行一组已规划的 checkout 子命令（共享循环）：逐条 spawn → 流式编码 →
/// 与取消/超时竞争 wait → 映射终态。host 与容器后端共用——只差 spawner。任一
/// 子命令非零退出/取消/超时即终止余下并返回对应终态。
///
/// 返回 `(终态, 可选容器名)`：取消/超时时携带在跑容器名（供调用方 `docker rm -f`
/// 补刀，ADR-0018）；正常退出/失败/spawn 失败时为 `None`（`--rm` 已自清或未起容器）。
//
// 参数多于 clippy 阈值：cmds/spawner 是执行输入、余下是 step 上下文（脱敏集/
// 截断/标识/取消/deadline/日志）——与 [`run`] 同款 allow。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_planned(
    cmds: &[PlannedCommand],
    secrets: Vec<Vec<u8>>,
    trunc: Arc<Truncation>,
    job_id: &str,
    attempt: i32,
    cancel_rx: watch::Receiver<bool>,
    deadline: Option<Instant>,
    logbuf: &LogBuffer,
    spawner: &dyn CommandSpawner,
) -> (JobOutcome, Option<String>) {
    for cmd in cmds {
        let env = env_map(&cmd.cred_env);
        let pipe_stdin = cmd.stdin.is_some();
        let (spawned, name) =
            match spawner.spawn(&cmd.program, &cmd.args, &cmd.cwd, &env, pipe_stdin) {
                Ok(p) => p,
                Err(SpawnError(e)) => {
                    return (JobOutcome::SpawnFailed(format!("checkout 失败：{e}")), None);
                }
            };
        // 每子命令取 job 级 deadline 的剩余配额（到点即 0 → 立即 timeout）。
        let remaining = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
        let outcome = stepio::run_streamed_step(
            spawned,
            cmd.stdin.clone(),
            secrets.clone(),
            trunc.clone(),
            job_id,
            attempt,
            remaining,
            cancel_rx.clone(),
            logbuf,
        )
        .await;
        match outcome {
            crate::exec::StepOutcome::Exited(0) => continue,
            crate::exec::StepOutcome::Exited(code) => {
                return (
                    JobOutcome::Failed(code, format!("{} 退出码 {code}", cmd.label)),
                    None,
                );
            }
            // 取消/超时：回传在跑容器名（调用方 docker rm -f 补刀；host 为 None）。
            crate::exec::StepOutcome::Cancelled => return (JobOutcome::Cancelled, name),
            crate::exec::StepOutcome::Timeout => return (JobOutcome::Timeout, name),
        }
    }
    (JobOutcome::Succeeded, None)
}

// ============================================================
// run：执行 checkout 子命令序列 → JobOutcome
// ============================================================

/// 执行 checkout 步骤。runner 在 step start 事件后调用；step end 事件由 runner
/// 据 [`JobOutcome`] 包裹。子命令输出经 [`crate::stepio`] 流式编码（脱敏 +
/// 截断）。返回终态：
/// - [`JobOutcome::Succeeded`]：全部子命令退出 0。
/// - [`JobOutcome::Failed`]：某子命令非零退出（携带退出码 + 命令名 detail）。
/// - [`JobOutcome::Cancelled`] / [`JobOutcome::Timeout`]：取消/超时终止余下命令。
/// - [`JobOutcome::SpawnFailed`]：缺二进制 / 规划错（detail 写明）。
///
/// `deadline` = job 级 deadline（每子命令取剩余配额）。`bins` = SCM 二进制
/// （默认 `git`/`svn`，测试注入不存在路径验缺二进制）。
//
// 参数多于 clippy 阈值：step/ws/credential/bins 是 checkout 输入、余下是 step
// 上下文（脱敏集/截断/标识/取消/deadline/日志）——皆 checkout 执行所需，未聚成
// 结构体以保调用点直观（与 grpc.rs 同款 allow）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    step: &CheckoutStep,
    ws_dir: &Path,
    credential: Option<&ScmCredential>,
    secrets: Vec<Vec<u8>>,
    trunc: Arc<Truncation>,
    job_id: &str,
    attempt: i32,
    cancel_rx: watch::Receiver<bool>,
    deadline: Option<Instant>,
    logbuf: &LogBuffer,
    bins: &ScmBins,
) -> JobOutcome {
    let vcs = match vcs_of(step) {
        Some(v) => v,
        None => return JobOutcome::SpawnFailed("checkout 失败：未知 VcsType".into()),
    };
    // 首次 vs 增量：检 .git/.svn 是否存在。
    let need_init = match vcs {
        VcsType::VcsGit => !ws_dir.join(".git").exists(),
        VcsType::VcsSvn => !ws_dir.join(".svn").exists(),
    };
    // git 凭据递送 artifact（svn 不用——svn 走 --username/stdin）。
    let cred = if matches!(vcs, VcsType::VcsGit) {
        match GitCredDelivery::prepare(credential, job_id, &step.repo_url) {
            Ok(c) => Some(c),
            Err(e) => {
                return JobOutcome::SpawnFailed(format!("checkout 失败：凭据递送准备失败：{e}"));
            }
        }
    } else {
        None
    };
    let cred_env = cred.as_ref().map(|c| c.env(credential)).unwrap_or_default();

    let plan_cmds = match plan(step, ws_dir, need_init, bins, &cred_env, credential) {
        Ok(p) => p,
        Err(e) => return JobOutcome::SpawnFailed(format!("checkout 失败：{e}")),
    };

    // 子命令执行循环（spawn → 流式编码 → 取消/超时竞争 → 映射终态）抽出为
    // [`run_planned`]：容器后端复用同一循环，只换 spawner（ADR-0018）。host 后端
    // 无容器名（`.0` 取终态，丢弃 `None` 名）。
    run_planned(
        &plan_cmds,
        secrets,
        trunc,
        job_id,
        attempt,
        cancel_rx,
        deadline,
        logbuf,
        &HostSpawner,
    )
    .await
    .0
}

/// `Vec<(String,String)>` → `HashMap`（`exec::spawn_command` 接口）。
fn env_map(pairs: &[(String, String)]) -> HashMap<String, String> {
    pairs.iter().cloned().collect()
}

// ============================================================
// 单元测试（纯规划 + 凭据 + 缺二进制 + 真实 git/svn）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use std::time::Duration;

    // ---- 测试夹具 ----

    fn bins() -> ScmBins {
        ScmBins::default()
    }

    fn git_step(url: &str, branch: &str, commit: &str, submodules: bool) -> CheckoutStep {
        CheckoutStep {
            vcs: VcsType::VcsGit as i32,
            repo_url: url.into(),
            r#ref: branch.into(),
            commit: commit.into(),
            submodules,
        }
    }

    fn svn_step(url: &str, rev: &str) -> CheckoutStep {
        CheckoutStep {
            vcs: VcsType::VcsSvn as i32,
            repo_url: url.into(),
            r#ref: rev.into(),
            commit: String::new(),
            submodules: false,
        }
    }

    fn cred(user: &str, pass: &str) -> ScmCredential {
        ScmCredential {
            username: user.into(),
            password: pass.into(),
        }
    }

    /// 某子命令的 args（拼成单行，便于断言「不含密码」）。
    fn args_line(cmd: &PlannedCommand) -> String {
        let mut s = cmd.program.clone();
        for a in &cmd.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }

    // ---- step_echo（脱敏摘要）----

    #[test]
    fn step_echo_git_omits_credentials() {
        let st = git_step("https://repo.example/org/proj", "main", "abc123", false);
        let echo = step_echo(&st);
        assert!(echo.contains("git checkout"));
        assert!(echo.contains("https://repo.example/org/proj"));
        assert!(echo.contains("abc123"), "含 commit 标识");
        assert!(!echo.contains("secret"), "无凭据");
    }

    #[test]
    fn step_echo_git_branch_head_when_no_commit() {
        let st = git_step("https://repo.example/org/proj", "dev", "", false);
        let echo = step_echo(&st);
        assert!(echo.contains("dev"), "无 commit 时回显分支");
    }

    #[test]
    fn step_echo_svn_shows_revision() {
        let st = svn_step("https://svn.example/repo", "42");
        let echo = step_echo(&st);
        assert!(echo.contains("svn checkout"));
        assert!(echo.contains("r42"), "含 revision");
    }

    // ---- plan_git（argv + 凭据不进 args）----

    #[test]
    fn plan_git_first_time_clones_then_pins() {
        let st = git_step("https://repo.example/p", "main", "abc123", false);
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], None).expect("plan");
        // clone → checkout --detach → reset --hard → clean -fd（无 submodule）。
        assert_eq!(cmds.len(), 4, "clone + checkout + reset + clean");
        assert_eq!(cmds[0].label, "git clone");
        assert!(cmds[0].args.contains(&"clone".into()));
        assert!(cmds[0].args.contains(&"https://repo.example/p".into()));
        assert_eq!(cmds[1].label, "git checkout --detach");
        assert!(cmds[1].args.contains(&"--detach".into()));
        assert!(cmds[1].args.contains(&"abc123".into()), "钉到 commit");
        assert_eq!(cmds[2].label, "git reset --hard");
        assert!(cmds[2].args.contains(&"--hard".into()));
        assert_eq!(cmds[3].label, "git clean -fd");
        assert!(cmds[3].args.contains(&"-fd".into()));
        assert!(
            !cmds[3].args.iter().any(|a| a == "-x"),
            "无 -x（保忽略文件）"
        );
        assert!(
            !cmds.iter().any(|c| c.label.contains("submodule")),
            "submodules=false 不加"
        );
    }

    #[test]
    fn plan_git_incremental_fetches_not_clones() {
        let st = git_step("https://repo.example/p", "main", "abc123", false);
        let cmds = plan(&st, Path::new("/ws"), false, &bins(), &[], None).expect("plan");
        assert_eq!(cmds[0].label, "git fetch");
        assert!(cmds[0].args.contains(&"fetch".into()));
        assert!(cmds[0].args.contains(&"origin".into()));
        assert!(
            cmds[0].args.contains(&"main".into()),
            "fetch origin <branch>"
        );
        // 后续钉点序列与首次同（checkout/reset/clean）。
        assert_eq!(cmds[1].label, "git checkout --detach");
    }

    #[test]
    fn plan_git_branch_head_when_no_commit() {
        let st = git_step("https://repo.example/p", "dev", "", false);
        let cmds = plan(&st, Path::new("/ws"), false, &bins(), &[], None).expect("plan");
        // checkout --detach origin/dev（分支头）。
        let checkout = cmds
            .iter()
            .find(|c| c.label == "git checkout --detach")
            .unwrap();
        assert!(
            checkout.args.contains(&"origin/dev".into()),
            "无 commit → origin/<branch>"
        );
        let reset = cmds.iter().find(|c| c.label == "git reset --hard").unwrap();
        assert!(reset.args.contains(&"origin/dev".into()));
    }

    #[test]
    fn plan_git_no_commit_no_branch_is_error() {
        let st = git_step("https://repo.example/p", "", "", false);
        let err = plan(&st, Path::new("/ws"), false, &bins(), &[], None).unwrap_err();
        assert!(
            err.contains("commit") || err.contains("branch"),
            "无法钉点：{err}"
        );
    }

    #[test]
    fn plan_git_submodules_on_adds_update() {
        let st = git_step("https://repo.example/p", "main", "abc123", true);
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], None).expect("plan");
        let sm = cmds
            .iter()
            .find(|c| c.label == "git submodule update --init --recursive");
        assert!(sm.is_some(), "submodules=true → submodule update");
        let sm = sm.unwrap();
        assert!(sm.args.contains(&"--init".into()));
        assert!(sm.args.contains(&"--recursive".into()));
    }

    #[test]
    fn plan_git_missing_repo_url_is_error() {
        let st = git_step("", "main", "abc123", false);
        let err = plan(&st, Path::new("/ws"), true, &bins(), &[], None).unwrap_err();
        assert!(err.contains("repo_url"), "{err}");
    }

    #[test]
    fn plan_git_credentials_never_in_args() {
        // 有凭据：cred_env 注入 git 命令，但 args 绝不含密码。
        let st = git_step("https://repo.example/p", "main", "abc123", true);
        let cred_env = git_cred_env(
            Some(&cred("alice", "super-secret-pw")),
            Some(Path::new("/tmp/helper.sh")),
        );
        let cmds = plan(
            &st,
            Path::new("/ws"),
            false,
            &bins(),
            &cred_env,
            Some(&cred("alice", "super-secret-pw")),
        )
        .expect("plan");
        for c in &cmds {
            let line = args_line(c);
            assert!(!line.contains("super-secret-pw"), "密码不得进 args：{line}");
            assert!(line.starts_with("git"), "program = git");
        }
        // cred_env 注入了每条 git 命令（ASKPASS env / credential store env，凭平台）。
        assert!(
            !cmds[0].cred_env.is_empty(),
            "git 命令带 cred_env（凭据经 env 递送，不进 args）"
        );
    }

    // ---- plan_svn（--password-from-stdin + stdin）----

    #[test]
    fn plan_svn_first_time_checkout_with_revision() {
        let st = svn_step("https://svn.example/repo", "42");
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], None).expect("plan");
        assert_eq!(cmds.len(), 1, "首次仅 checkout");
        assert_eq!(cmds[0].label, "svn checkout");
        assert!(cmds[0].args.contains(&"checkout".into()));
        assert!(cmds[0].args.contains(&"-r".into()));
        assert!(cmds[0].args.contains(&"42".into()));
        assert!(cmds[0].args.contains(&"https://svn.example/repo".into()));
        assert!(cmds[0].stdin.is_none(), "无凭据 → 不接管 stdin");
    }

    #[test]
    fn plan_svn_incremental_cleanup_then_update() {
        let st = svn_step("https://svn.example/repo", "42");
        let cmds = plan(&st, Path::new("/ws"), false, &bins(), &[], None).expect("plan");
        assert_eq!(cmds.len(), 2, "cleanup + update");
        assert_eq!(cmds[0].label, "svn cleanup");
        assert_eq!(cmds[1].label, "svn update");
        assert!(cmds[1].args.contains(&"-r".into()));
        assert!(cmds[1].args.contains(&"42".into()));
    }

    #[test]
    fn plan_svn_no_revision_targets_head() {
        let st = svn_step("https://svn.example/repo", "");
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], None).expect("plan");
        assert!(
            !cmds[0].args.contains(&"-r".into()),
            "rev 空 → 不带 -r（HEAD）"
        );
    }

    #[test]
    fn plan_svn_credentials_via_stdin_not_args() {
        let st = svn_step("https://svn.example/repo", "42");
        let c = cred("bob", "svn-secret-pw");
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], Some(&c)).expect("plan");
        let line = args_line(&cmds[0]);
        assert!(!line.contains("svn-secret-pw"), "密码不得进 args：{line}");
        assert!(cmds[0].args.contains(&"--username".into()));
        assert!(
            cmds[0].args.contains(&"bob".into()),
            "username 在 args（非机密）"
        );
        assert!(cmds[0].args.contains(&"--password-from-stdin".into()));
        // stdin = 密码 + 换行。
        let stdin = cmds[0].stdin.as_ref().expect("有凭据 → stdin");
        assert_eq!(stdin, b"svn-secret-pw\n", "密码经 stdin 递送");
    }

    #[test]
    fn plan_svn_no_credential_no_auth_args() {
        let st = svn_step("https://svn.example/repo", "1");
        let cmds = plan(&st, Path::new("/ws"), true, &bins(), &[], None).expect("plan");
        assert!(!cmds[0].args.contains(&"--username".into()));
        assert!(!cmds[0].args.contains(&"--password-from-stdin".into()));
        assert!(cmds[0].stdin.is_none());
    }

    #[test]
    fn plan_unknown_vcs_is_error() {
        let mut st = git_step("https://repo.example/p", "main", "abc", false);
        st.vcs = 999; // 越界
        let err = plan(&st, Path::new("/ws"), true, &bins(), &[], None).unwrap_err();
        assert!(err.contains("VcsType"), "{err}");
    }

    // ---- 凭据递送纯函数 ----

    #[test]
    fn askpass_helper_script_has_no_credentials() {
        let script = askpass_helper_script();
        assert!(script.starts_with("#!/bin/sh"), "shebang 使可 invoke");
        assert!(script.contains("SISY_SCM_USER"), "读 env 取用户名");
        assert!(script.contains("SISY_SCM_PASS"), "读 env 取密码");
        // 脚本静态、不含任何具体凭据字面量。
        assert!(!script.contains("password="));
    }

    #[test]
    fn credstore_content_formats_user_pass_host() {
        let s = credstore_content("alice", "p@ss", "github.com");
        assert_eq!(s, "https://alice:p@ss@github.com\n");
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
            Some("host.com".into()),
            "容忍 user@host"
        );
        assert_eq!(
            https_host("git@github.com:org/repo.git"),
            None,
            "ssh 非 https"
        );
        assert_eq!(https_host("file:///path/to/repo"), None, "file 非 https");
    }

    #[test]
    fn git_cred_env_includes_askpass_on_unix_or_config_on_windows() {
        let c = cred("alice", "pw");
        let env = git_cred_env(Some(&c), Some(Path::new("/tmp/sisyphus-scm-job1.sh")));
        // 两平台都把凭据递送信息放 env（不进 args），密码不在 args 已由 plan 测试覆盖。
        assert!(!env.is_empty(), "有凭据 + artifact → 非空 env");
        if cfg!(unix) {
            assert!(
                env.iter().any(|(k, _)| k == "GIT_ASKPASS"),
                "Unix: GIT_ASKPASS"
            );
            assert!(
                env.iter().any(|(k, _)| k == "SISY_SCM_PASS"),
                "Unix: 密码在 env（非 args）"
            );
            assert!(
                env.iter()
                    .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v == "0")
            );
        } else {
            // Windows credential store：GIT_CONFIG_KEY_0=credential.helper、
            // VALUE_0=store --file=<path>（密码在文件、不在 env/args）。
            assert!(
                env.iter()
                    .any(|(k, v)| k == "GIT_CONFIG_KEY_0" && v == "credential.helper")
            );
            assert!(
                env.iter()
                    .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v.contains("store --file="))
            );
        }
    }

    #[test]
    fn git_cred_env_empty_without_credential() {
        assert!(git_cred_env(None, Some(Path::new("/tmp/x"))).is_empty());
        assert!(git_cred_env(Some(&cred("u", "p")), None).is_empty());
    }

    // ---- GitCredDelivery 生命周期（写盘 + Drop 清理）----

    #[test]
    fn git_cred_delivery_prepared_and_cleaned_on_drop() {
        // prepare 写 artifact 到系统 temp；用 job_id 唯一化避免并发测试互踩。
        let job = format!("delivery-test-{}", std::process::id());
        let c = cred("alice", "pw");
        let prepared = GitCredDelivery::prepare(Some(&c), &job, "https://github.com/org/repo")
            .expect("prepare");
        let path = prepared.artifact.clone().expect("有凭据 → artifact 已写");
        assert!(path.exists(), "artifact 文件已创建");
        // Windows：经 icacls 限制 ACL 后，dump 不应含 Everyone / BUILTIN\Users
        // （0600 等价：仅当前用户）。Unix：0700 在 ASKPASS 路径，权限断言由 mode 测。
        #[cfg(windows)]
        {
            let dump = std::process::Command::new("icacls")
                .arg(&path)
                .output()
                .expect("icacls dump");
            assert!(dump.status.success(), "icacls dump 失败");
            let acl = String::from_utf8_lossy(&dump.stdout);
            assert!(!acl.contains("Everyone"), "ACL 不应含 Everyone：{acl}");
            assert!(
                !acl.contains("BUILTIN\\Users"),
                "ACL 不应含 BUILTIN\\Users：{acl}"
            );
        }
        // Drop → 清理。
        drop(prepared);
        assert!(!path.exists(), "Drop 删除 artifact");
    }

    #[test]
    fn git_cred_delivery_no_artifact_without_credential() {
        let prepared = GitCredDelivery::prepare(None, "job", "https://x").expect("prepare");
        assert!(prepared.artifact.is_none(), "无凭据 → 无 artifact");
        assert!(prepared.env(None).is_empty(), "无凭据 → 空 env");
    }

    // ---- run：缺二进制清晰报错 ----

    fn logbuf(dir: &Path) -> LogBuffer {
        let p = dir.join("logbuf");
        std::fs::create_dir_all(&p).expect("建 logbuf 目录");
        LogBuffer::new(p, Duration::from_secs(60))
    }

    #[tokio::test]
    async fn run_missing_git_binary_reports_spawn_failed() {
        let dir = tempfile::tempdir().expect("临时目录");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step("https://repo.example/p", "main", "abc123", false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let bins = ScmBins {
            git: "sisyphus-no-such-git-zzz".into(),
            svn: "svn".into(),
        };
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job",
            0,
            cancel,
            None,
            &lb,
            &bins,
        )
        .await;
        match outcome {
            JobOutcome::SpawnFailed(d) => assert!(
                d.contains("缺") && d.contains("git"),
                "缺二进制清晰报错：{d}"
            ),
            other => panic!("缺 git 应 SpawnFailed，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_missing_repo_url_reports_spawn_failed() {
        let dir = tempfile::tempdir().expect("临时目录");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step("", "main", "abc123", false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        match outcome {
            JobOutcome::SpawnFailed(d) => assert!(d.contains("repo_url"), "缺 url 报错：{d}"),
            other => panic!("缺 url 应 SpawnFailed，实际 {other:?}"),
        }
    }

    // ---- 真实 git 集成（本地仓库）----

    /// 创建本地 git 仓库并返回其绝对路径 + HEAD commit sha。仓库内含一个提交文件。
    fn make_git_repo(parent: &Path, name: &str) -> (PathBuf, String) {
        let repo = parent.join(name);
        std::fs::create_dir_all(&repo).expect("建 repo 目录");
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .args(args)
                .current_dir(&repo)
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
        // 隔离测试仓库配置（不污染全局）+ 设置提交者身份。`git symbolic-ref` 把
        // HEAD 指到 main（可移植：不依赖 2.28+ 的 --initial-branch）。
        git(&["init", "--quiet"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&["config", "user.email", "test@sisyphus.local"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("hello.txt"), "v1\n").expect("写文件");
        git(&["add", "hello.txt"]);
        git(&["commit", "--quiet", "-m", "v1"]);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("utf8")
            .trim()
            .to_string();
        (repo, sha)
    }

    /// 工作区目录里跑 `git rev-parse HEAD` 取当前 HEAD sha。
    fn ws_head(ws: &Path) -> String {
        let out = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(ws)
            .output()
            .expect("git rev-parse");
        assert!(
            out.status.success(),
            "rev-parse 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn git_run_clones_and_pins_to_commit() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step(&repo.to_string_lossy(), "main", &sha, false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-clone",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "clone + pin 应成功");
        assert_eq!(ws_head(&ws), sha, "HEAD 钉到 commit");
        assert!(ws.join("hello.txt").is_file(), "文件已检出");
        assert!(ws.join(".git").is_dir(), ".git 保留（增量复用前提）");
    }

    /// AC「凭据不进日志」：带凭据跑 checkout（file:// 无需认证，但凭据经
    /// cred_env/credstore 递送）后，回放 logbuf 扫描全部输出块——密码不得出现。
    /// 也断言 step_echo（runner 层命令回显）不含密码。证明凭据递送不泄进日志。
    #[tokio::test]
    async fn git_run_with_credential_does_not_leak_password_in_output() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step(&repo.to_string_lossy(), "main", &sha, false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        // 带凭据 + 把密码纳入脱敏集（runner collect_secrets 同道）。
        let c = cred("alice", "hunter2-secret-pw");
        let outcome = run(
            &st,
            &ws,
            Some(&c),
            vec![b"hunter2-secret-pw".to_vec()],
            trunc,
            "job-cred",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "带凭据 checkout 应成功");
        // 回放 logbuf：扫描所有输出块，密码不得出现。
        let batches = lb.replay("job-cred", 0).await.expect("replay logbuf");
        let mut all_output = Vec::new();
        for msg in &batches {
            if let Some(sisyphus_proto::agent::channel_message::Kind::LogBatch(b)) = &msg.kind {
                for ev in &b.events {
                    if let Some(sisyphus_proto::agent::log_event::Kind::Output(o)) = &ev.kind {
                        all_output.extend_from_slice(&o.data);
                    }
                }
            }
        }
        let out = String::from_utf8_lossy(&all_output);
        assert!(
            !out.contains("hunter2-secret-pw"),
            "密码不得泄进日志输出：{out}"
        );
        // step_echo（runner 命令回显）不含密码（只用 repo_url + 目标）。
        assert!(
            !step_echo(&st).contains("hunter2-secret-pw"),
            "step_echo 不含密码：{}",
            step_echo(&st)
        );
    }

    #[tokio::test]
    async fn git_run_incremental_resets_and_cleans() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step(&repo.to_string_lossy(), "main", &sha, false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));

        // 首次 checkout。
        let (_tx, cancel) = watch::channel(false);
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc.clone(),
            "job-inc",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded);

        // 脏化工作区：修改已跟踪文件 + 加未跟踪文件。
        std::fs::write(ws.join("hello.txt"), "dirty\n").expect("改跟踪文件");
        std::fs::write(ws.join("untracked.txt"), "junk\n").expect("加未跟踪文件");

        // 再跑同一 commit（增量：fetch + reset --hard + clean -fd）。
        let (_tx, cancel) = watch::channel(false);
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-inc",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "增量应成功");
        assert_eq!(ws_head(&ws), sha, "HEAD 仍钉到 commit");
        assert_eq!(
            std::fs::read_to_string(ws.join("hello.txt")).unwrap(),
            "v1\n",
            "reset --hard 还原跟踪文件"
        );
        assert!(!ws.join("untracked.txt").exists(), "clean -fd 删未跟踪文件");
    }

    #[tokio::test]
    async fn git_run_branch_head_when_no_commit() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, _sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        // commit 空 → Agent 检分支头 origin/main。
        let st = git_step(&repo.to_string_lossy(), "main", "", false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-br",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "无 commit → 分支头应成功");
        // HEAD 应是 origin/main 指向的同一 commit（main 的头）。
        let head = ws_head(&ws);
        let repo_head = String::from_utf8(
            StdCommand::new("git")
                .args(["rev-parse", "main"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(head, repo_head, "HEAD = 远端 main 头");
    }

    #[tokio::test]
    async fn git_run_submodules_off_skips_update() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let st = git_step(&repo.to_string_lossy(), "main", &sha, false);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-sm-off",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "submodules=false 应成功");
        // .gitmodules 不存在（仓库无子模块）；无 submodule update 命令也无副作用。
        assert_eq!(ws_head(&ws), sha);
    }

    /// submodules=true：`git submodule update --init --recursive` 命令执行（无子模块
    /// 时是 no-op 退出 0）。与 plan 级测试（submodules=true → 该命令在规划中、
    /// false → 不在）合覆 AC「默认开、按步骤开关关闭时跳过」。
    ///
    /// 不在此跑「真实子模块初始化」端到端：git 2.38+ 的 `protocol.file.allow=user`
    /// 默认（CVE-2022-39253）禁止 file:// 子模块克隆，本地 fixture 起子模块会被
    /// 拒；真实 https/ssh 子模块需网络。命令编排（`--init --recursive`）正确性由
    /// plan 级测试保证、命令执行不报错由本 no-op 测试保证。
    #[tokio::test]
    async fn git_run_submodules_on_runs_init_update() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (repo, sha) = make_git_repo(dir.path(), "src-repo");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        // submodules=true：规划含 submodule update --init --recursive；无子模块时它
        // no-op 退出 0。若命令参数拼错会非零退出 → 此处失败。
        let st = git_step(&repo.to_string_lossy(), "main", &sha, true);
        let (_tx, cancel) = watch::channel(false);
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-sm-on",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(
            outcome,
            JobOutcome::Succeeded,
            "submodules=true：submodule update 命令应执行成功（无子模块时 no-op）"
        );
        assert_eq!(ws_head(&ws), sha);
    }

    // ---- 真实 svn 集成（不可用则 skip）----

    /// svn 是否可用（`svn --version` 与 `svnadmin --version` 均可执行）。
    fn svn_available() -> bool {
        StdCommand::new("svn")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
            && StdCommand::new("svnadmin")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
    }

    /// 创建本地 svn 仓库（svnadmin create）并 import 一个文件到 `/trunk`，
    /// 产生 revision 1（`svn import` 一并创建 `/trunk` 路径）。返回可 checkout 的
    /// `file://` URL（前导盘符已转正斜杠）。
    fn make_svn_repo(parent: &Path, name: &str) -> String {
        let repo = parent.join(name);
        let _ = StdCommand::new("svnadmin")
            .args(["create", &repo.to_string_lossy()])
            .output()
            .expect("svnadmin create");
        // import 一个文件到 /trunk 产生 r1（import 一并创建 /trunk）。
        let src = parent.join("svn-import-src");
        std::fs::create_dir_all(&src).expect("建 import 源");
        std::fs::write(src.join("hello.txt"), "svn-v1\n").expect("写文件");
        let url = file_url(&repo);
        let imp = StdCommand::new("svn")
            .args([
                "import",
                "--quiet",
                "-m",
                "v1",
                &src.to_string_lossy(),
                &format!("{url}/trunk"),
            ])
            .output()
            .expect("svn import");
        assert!(
            imp.status.success(),
            "svn import 失败：{}",
            String::from_utf8_lossy(&imp.stderr)
        );
        url
    }

    /// 本地路径 → `file://` URL（Windows 盘符反斜杠转正斜杠、去前导斜杠避免三斜杠后多一截）。
    fn file_url(path: &Path) -> String {
        let s = path.to_string_lossy().replace('\\', "/");
        let s = s.trim_start_matches('/');
        format!("file:///{s}")
    }

    /// 工作区当前 svn revision（`svn info --show-item revision`，svn ≥ 1.9）。
    fn svn_info_rev(ws: &Path) -> String {
        let out = StdCommand::new("svn")
            .args(["info", "--show-item", "revision", &ws.to_string_lossy()])
            .output()
            .expect("svn info");
        assert!(
            out.status.success(),
            "svn info 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn svn_run_checkout_and_update() {
        if !svn_available() {
            eprintln!("skip: svn 不可用");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let repo_url = make_svn_repo(dir.path(), "svn-repo");
        let trunk = format!("{repo_url}/trunk");
        // 造 r2：再 import 一个文件到 /trunk（r1 只有 hello.txt；r2 加 world.txt）。
        let src2 = dir.path().join("svn-import-src2");
        std::fs::create_dir_all(&src2).expect("建 import 源 2");
        std::fs::write(src2.join("world.txt"), "svn-v2\n").expect("写文件 2");
        let imp2 = StdCommand::new("svn")
            .args([
                "import",
                "--quiet",
                "-m",
                "v2",
                &src2.to_string_lossy(),
                &format!("{trunk}/world.txt"),
            ])
            .output()
            .expect("svn import 2");
        assert!(
            imp2.status.success(),
            "svn import r2 失败：{}",
            String::from_utf8_lossy(&imp2.stderr)
        );

        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).expect("建工作区");
        let lb = logbuf(dir.path());
        let trunc = Arc::new(Truncation::new(u64::MAX));

        // 首次 checkout -r 1：钉到 r1（hello.txt 在、r2 的 world.txt 不在）。
        let st = svn_step(&trunk, "1");
        let (_tx, cancel) = watch::channel(false);
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc.clone(),
            "job-svn",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "svn checkout 应成功");
        assert!(ws.join("hello.txt").is_file(), "r1 的文件已检出");
        assert!(!ws.join("world.txt").exists(), "钉到 r1：r2 的文件不在");
        assert!(ws.join(".svn").is_dir(), ".svn 保留（增量复用前提）");
        assert_eq!(svn_info_rev(&ws), "1", "WC 钉到 r1");

        // 增量 update -r 1（cleanup + update）：仍钉 r1（不拉 r2 的 world.txt）。
        let (_tx, cancel) = watch::channel(false);
        let outcome = run(
            &st,
            &ws,
            None,
            vec![],
            trunc,
            "job-svn",
            0,
            cancel,
            None,
            &lb,
            &bins(),
        )
        .await;
        assert_eq!(outcome, JobOutcome::Succeeded, "svn 增量应成功");
        assert!(
            !ws.join("world.txt").exists(),
            "增量 update -r 1 仍钉 r1（不拉 r2）"
        );
        assert_eq!(svn_info_rev(&ws), "1", "增量后 WC 仍 r1");
    }
}

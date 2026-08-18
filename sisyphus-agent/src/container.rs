//! runner 容器后端（ADR-0018；票 B3-T7 / #53）。
//!
//! Agent 侧 docker 容器后端：每步骤一次 `docker run --rm`，shell 出系统 `docker`
//! CLI（与 ADR-0016 shell 出 git/svn 同模型，不内嵌容器库）。本模块管「容器怎么
//! 起、怎么清」：命令装配（纯函数，单测面）、镜像 pull、临时 env 文件 + ASKPASS
//! 挂载、取消/超时 `docker rm -f` 补刀、启动按 label 清扫残留容器。步骤编排
//! （JobSpec → 步骤序贯 → 终态）、日志事件编码、机密脱敏、`${SISY_WORKSPACE}`
//! 替换归 [`crate::runner`]；本模块不碰 proto/logbuf 的步骤面（checkout 复用
//! [`crate::checkout::plan`] + [`crate::checkout::run_planned`]）。
//!
//! - **命令装配**（[`assemble_run`]）：`docker run --rm -v <ws>:/sisyphus/workspace
//!   -w <workdir> [--user uid:gid] --env-file <f> [-v <askpass>:/sisyphus/askpass.sh:ro]
//!   --name <name> --label ... <image> <command...>`。纯函数，无 IO。
//! - **挂载/路径/入口全固定**（ADR-0018）：工作区挂载 `/sisyphus/workspace`、
//!   入口 `/bin/sh -c`、HOME 重定向 `/sisyphus/workspace/.sisyphus-home`。容器配置
//!   仅 `image` 一个字段；无额外挂载/privileged/network 透出。
//! - **容器用户**（Linux 宿主）：`--user <agent uid:gid>` 把 Agent 自身 uid/gid 映射
//!   进容器，避免容器内 root 在挂载工作区落盘卡死宿主侧缓存 save/工作区清理。
//!   macOS/Windows 宿主 as-is（不带 `--user`，Docker Desktop 类 Linux 引擎自行翻译）。
//! - **env 递送**（机密 + SCM 凭据 + 任务 env）统一走临时 env 文件（`--env-file`，
//!   0600、任务毕即删、不上命令行）；HOME 重定向写入 env 文件（覆盖用户 HOME）。
//! - **ASKPASS**（git 凭据，容器内 checkout）：静态助手脚本挂载 `/sisyphus/askpass.sh`
//!   （只读），`GIT_ASKPASS=/sisyphus/askpass.sh` + `SISY_SCM_USER/PASS` 进 env 文件。
//! - **镜像拉取**：任务首步前显式 `docker pull`（always）；失败 = 任务失败、清晰报错。
//! - **取消/超时补刀**：杀掉 CLI 进程树后按名 `docker rm -f`（幂等，`--rm` 已清则
//!   忽略 No such container）。
//! - **启动清扫**：[`cleanup_orphan_containers`] 按 `sisyphus.managed=true` label
//!   清扫残留容器（兜住 CLI 被 SIGKILL 的窗口）。
//! - **探测**：[`crate::channel::ContainerProbe`] 周期 `docker version` →
//!   `sisyphus/container=docker` 标签随 metadata 上报（ADR-0008）。
//!
//! 真实 docker 执行（pull/run/清扫/探测）单测门控 `#[ignore]`（需 daemon）；装配
//! 与纯助手无 daemon 即绿。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sisyphus_proto::agent::{CheckoutStep, ContainerEnv, JobSpec, ScmCredential, VcsType};
use tokio::sync::watch;

use crate::checkout;
use crate::exec::SpawnError;
use crate::logbuf::LogBuffer;
use crate::runner::JobOutcome;
use crate::stepio::Truncation;

// ============================================================
// 固定挂载/路径/标签（ADR-0018：不可配）
// ============================================================

/// 工作区在容器内的挂载点（固定，ADR-0018）。`pub(crate)`：runner 的
/// `${SISY_WORKSPACE}` 展开根（container 分支）读此常量。
pub(crate) const WORKSPACE_MOUNT_TARGET: &str = "/sisyphus/workspace";
/// ASKPASS 助手脚本在容器内的挂载点（git 凭据递送，ADR-0018）。仅本模块用。
const ASKPASS_MOUNT_TARGET: &str = "/sisyphus/askpass.sh";
/// HOME 重定向目标（容器内，工作区下隐藏目录；跨步骤持久、随工作区清理回收）。
const HOME_IN_CONTAINER: &str = "/sisyphus/workspace/.sisyphus-home";

/// 归属 label（标记为 sisyphus 托管）：`sisyphus.managed=true`。用于 `--label` 与
/// 启动清扫的 `--filter label=...`。
const LABEL_MANAGED: &str = "sisyphus.managed=true";
/// 归属 label 的 key（`sisyphus.managed`）。
const LABEL_MANAGED_KEY: &str = "sisyphus.managed";
/// 归属 label 的 value（`true`）。
const LABEL_MANAGED_VAL: &str = "true";
/// 任务回溯 label 的 key（`sisyphus.job`），值为 job_id。
const LABEL_JOB_KEY: &str = "sisyphus.job";

/// docker CLI 二进制名（默认 PATH 查找；将来 Agent 配置可覆写为绝对路径）。
pub const DOCKER_BIN: &str = "docker";

// ============================================================
// docker run 命令装配（纯函数，单测面）
// ============================================================

/// `docker run` 装配入参（纯数据，无 IO）。[`assemble_run`] 据此产出完整 argv，
/// 交 [`crate::exec::spawn_command`] 以 `docker` 为 program 起进程。
#[derive(Debug, Clone)]
pub(crate) struct RunSpec {
    /// 容器镜像（`ContainerEnv.image`，v1 仅此一个字段）。
    pub image: String,
    /// 宿主侧 env 文件路径（`--env-file`；机密 + SCM 凭据 + 任务 env，0600）。
    pub env_file: PathBuf,
    /// 宿主侧工作区路径（挂载源 → `/sisyphus/workspace`）。
    pub workspace_host: PathBuf,
    /// 容器内工作目录（`-w`）。shell 步骤 = `/sisyphus/workspace`；checkout 子命令
    /// 取规划 cwd（clone 用 `/sisyphus`，其余 `/sisyphus/workspace`）。
    pub workdir: String,
    /// `--user <uid:gid>`（Linux 宿主；None = 不带，macOS/Windows as-is）。
    pub user: Option<(u32, u32)>,
    /// 宿主侧 ASKPASS 助手脚本路径（git 凭据；None = 无 SCM 凭据，不挂载）。
    pub askpass_host: Option<PathBuf>,
    /// 容器名 `sisyphus-<job>-<attempt>-<stepseq>-<短随机>`（+ 归属 label）。
    pub name: String,
    /// 归属 labels（`--label k=v`）：`sisyphus.managed=true` + `sisyphus.job=<id>`。
    pub labels: Vec<(String, String)>,
    /// 入口 + 参数（image 之后的 argv）：shell 步骤 `["/bin/sh","-c","<cmd>"]`；
    /// checkout 子命令 `["git","clone",...]` 等。
    pub command: Vec<String>,
}

/// 装配 `docker run` 的完整 argv（纯函数，无 IO、不 spawn）。crate 内 seam：
/// [`ContainerTask::run_spec`] 构造 [`RunSpec`]，[`ContainerTask::spawn_run`] 起进程。
///
/// 顺序：`run --rm -v <ws>:/sisyphus/workspace -w <workdir> [--user uid:gid]
/// --env-file <f> [-v <askpass>:/sisyphus/askpass.sh:ro] --name <name>
/// --label <k=v>... <image> <command...>`。各 flag 顺序对 docker 无语义影响，
/// 单测按此断言。
pub(crate) fn assemble_run(spec: &RunSpec) -> Vec<String> {
    let mut args = vec!["run".to_string(), "--rm".to_string()];
    // 工作区挂载（rw 默认：容器写工作区；缓存 restore/save 在挂载目录上）。
    args.push("-v".into());
    args.push(format!(
        "{}:{}",
        spec.workspace_host.display(),
        WORKSPACE_MOUNT_TARGET
    ));
    // 工作目录。
    args.push("-w".into());
    args.push(spec.workdir.clone());
    // 容器用户（Linux 宿主：映射 Agent uid/gid）。
    if let Some((uid, gid)) = spec.user {
        args.push("--user".into());
        args.push(format!("{uid}:{gid}"));
    }
    // env 文件（机密 + SCM 凭据 + 任务 env，经文件递送不上命令行）。
    args.push("--env-file".into());
    args.push(spec.env_file.to_string_lossy().into_owned());
    // ASKPASS 助手脚本（git 凭据，只读挂载）。
    if let Some(askpass) = &spec.askpass_host {
        args.push("-v".into());
        args.push(format!("{}:{}:ro", askpass.display(), ASKPASS_MOUNT_TARGET));
    }
    // 容器名 + 归属 label。
    args.push("--name".into());
    args.push(spec.name.clone());
    for (k, v) in &spec.labels {
        args.push("--label".into());
        args.push(format!("{k}={v}"));
    }
    // 镜像 + 入口/参数。
    args.push(spec.image.clone());
    args.extend(spec.command.iter().cloned());
    args
}

// ============================================================
// 容器命名（纯）
// ============================================================

/// 容器名：`sisyphus-<job>-<attempt>-<stepseq>-<短随机>`。job_id 经
/// [`sanitize_name`] 清洗为容器名合法字符（`[a-zA-Z0-9_.-]`，余者→`_`）。`sisyphus-`
/// 前缀保证首字符为字母（容器名要求 `[a-zA-Z0-9][a-zA-Z0-9_.-]*`）。
pub(crate) fn container_name(job_id: &str, attempt: i32, step_seq: i32, suffix: &str) -> String {
    format!(
        "sisyphus-{}-{attempt}-{step_seq}-{suffix}",
        sanitize_name(job_id)
    )
}

/// 清洗为容器名合法字符：`[a-zA-Z0-9_.-]` 保留，余者→`_`；空串→`_`。
fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// 短随机后缀（8 位 hex）：xorshift 混合（纳秒 × 常数 + 单调计数器）的低位。
/// 跨步骤/跨任务避免同名（兜住前次崩溃留下的同名孤儿容器）；无 `rand` 依赖
/// （与 `channel::Backoff` 同款自混合）。
fn short_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mix = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(n)
        .wrapping_add(0x6C62_272E_EF84_E493);
    format!("{:08x}", mix & 0xFFFF_FFFF)
}

// ============================================================
// 临时 env 文件（机密 + SCM 凭据 + 任务 env；0600、任务毕即删）
// ============================================================

/// 写临时 env 文件：`KEY=VALUE` 逐行（docker `--env-file` 按行读，值取字面量——
/// 不展开变量、`#` 仅行首为注释）。HOME 自 spec.env 滤除后追加重定向值（ADR-0018：
/// HOME 指向工作区内 `.sisyphus-home`，覆盖用户 HOME）；有 SCM 凭据时追加
/// git ASKPASS env（容器内 checkout 用）。0600（Unix）。
pub(crate) fn write_env_file(
    path: &Path,
    spec_env: &HashMap<String, String>,
    scm: Option<&ScmCredential>,
) -> io::Result<()> {
    let mut lines: Vec<String> = Vec::new();
    for (k, v) in spec_env {
        // HOME 重定向：丢弃用户 HOME，由末尾重定向值覆盖。
        if k == "HOME" {
            continue;
        }
        lines.push(env_line(k, v));
    }
    lines.push(env_line("HOME", HOME_IN_CONTAINER));
    if let Some(c) = scm {
        lines.push(env_line("GIT_ASKPASS", ASKPASS_MOUNT_TARGET));
        lines.push(env_line("SISY_SCM_USER", &c.username));
        lines.push(env_line("SISY_SCM_PASS", &c.password));
        lines.push(env_line("GIT_TERMINAL_PROMPT", "0"));
    }
    let mut content = lines.join("\n");
    content.push('\n');
    std::fs::write(path, content)?;
    set_mode(path, 0o600)
}

/// 一行 `KEY=VALUE`（值取字面量；docker `--env-file` 不展开变量、`#` 仅行首注释）。
fn env_line(k: &str, v: &str) -> String {
    format!("{k}={v}")
}

// ============================================================
// ASKPASS 助手脚本（git 凭据，容器内 checkout；静态、不含凭据）
// ============================================================

/// GIT_ASKPASS 助手脚本内容（与 [`crate::checkout`] 同源；容器内 /bin/sh 读
/// `$SISY_SCM_USER`/`$SISY_SCM_PASS`——经 env 文件注入容器环境，由 helper 继承）。
/// 静态、不含任何凭据字面量。
pub(crate) fn askpass_script() -> &'static str {
    "#!/bin/sh\ncase \"$1\" in\n*Username*) echo \"$SISY_SCM_USER\" ;;\n*) echo \"$SISY_SCM_PASS\" ;;\nesac\n"
}

/// 写 ASKPASS 助手脚本到宿主临时路径（0700，使容器内 git 可直接 invoke）。
pub(crate) fn write_askpass(path: &Path) -> io::Result<()> {
    std::fs::write(path, askpass_script())?;
    set_mode(path, 0o700)
}

// ============================================================
// 文件权限（Unix chmod；其它平台 no-op）
// ============================================================

/// Unix chmod；其它平台 no-op（容器 v1 仅承诺 Linux 宿主，macOS/Windows as-is）。
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

// ============================================================
// 启动清扫残留容器（按 label；best-effort，无 docker 即跳过）
// ============================================================

/// 启动时按 `sisyphus.managed=true` label 清扫残留容器（兜住 CLI 被 SIGKILL 的
/// 窗口留下的孤儿）。best-effort：无 docker / 列举失败 → 跳过（返回 Ok）；
/// 逐个 `docker rm -f`，失败忽略（幂等——不存在的容器报错不影响其余）。
pub async fn cleanup_orphan_containers(docker_bin: &str) -> io::Result<()> {
    let ids = list_orphan_containers(docker_bin).await.unwrap_or_default();
    for id in ids {
        // 幂等补刀：No such container 报错忽略，继续其余。
        let _ = rm_container(docker_bin, &id).await;
    }
    Ok(())
}

/// 列出 sisyphus 托管的残留容器 ID（`docker ps -a --filter label=... -q`）。
/// 无 docker / spawn 失败 → Err（调用方 best-effort 跳过）。
async fn list_orphan_containers(docker_bin: &str) -> io::Result<Vec<String>> {
    let out = tokio::process::Command::new(docker_bin)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL_MANAGED}"),
            "-q",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// `docker rm -f <id_or_name>`（幂等：失败忽略）。取消/超时补刀与启动清扫共用。
async fn rm_container(docker_bin: &str, id_or_name: &str) -> io::Result<()> {
    let _ = tokio::process::Command::new(docker_bin)
        .args(["rm", "-f", id_or_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    Ok(())
}

// ============================================================
// ContainerTask：容器任务的 per-task 上下文 + 执行面
// ============================================================

/// 容器任务上下文（per-task）：镜像、临时 env 文件、ASKPASS 挂载、容器用户、
/// 命名前缀、归属 labels、docker 二进制。由 [`ContainerTask::prepare`] 装配
/// （写 env 文件 + ASKPASS + `.sisyphus-home` 目录），[`Drop`] 时清理临时文件
/// （任务毕即删，ADR-0018）。
pub(crate) struct ContainerTask {
    /// 容器镜像（`ContainerEnv.image`）。
    image: String,
    /// 宿主侧临时 env 文件（机密 + SCM 凭据 + 任务 env，0600）。
    env_file: PathBuf,
    /// 宿主侧 ASKPASS 助手脚本（有 SCM 凭据时，0700）；None = 不挂载。
    askpass: Option<PathBuf>,
    /// 宿主侧工作区路径（挂载源 → `/sisyphus/workspace`）。
    ws_host: PathBuf,
    /// `--user <uid:gid>`（Linux 宿主）；None = 不带（macOS/Windows as-is）。
    user: Option<(u32, u32)>,
    /// job_id（容器名 + 归属 label 用）。
    job_id: String,
    /// attempt（容器名用）。
    attempt: i32,
    /// 归属 labels（`sisyphus.managed=true` + `sisyphus.job=<id>`）。
    labels: Vec<(String, String)>,
    /// docker CLI 二进制名（默认 [`DOCKER_BIN`]）。
    docker_bin: String,
}

impl ContainerTask {
    /// 装配容器任务上下文：写 env 文件（机密 + SCM 凭据 + 任务 env，0600）+
    /// ASKPASS 助手脚本（有 SCM 凭据时，0700）+ `.sisyphus-home` 目录 + 容器用户
    /// （Linux）+ 命名前缀 + 归属 labels。**不 pull**（pull 在 [`crate::runner`]
    /// 首步前）。`image` 空 / 写盘失败 → Err（detail 清晰）。
    pub(crate) fn prepare(c: &ContainerEnv, spec: &JobSpec, ws_dir: &Path) -> Result<Self, String> {
        let image = c.image.trim().to_string();
        if image.is_empty() {
            return Err("容器任务缺 image（ContainerEnv.image 为空）".into());
        }
        let env_file = temp_path(&spec.job_id, spec.attempt, "env");
        let scm = spec.scm_credential.as_ref();
        if let Err(e) = write_env_file(&env_file, &spec.env, scm) {
            let _ = std::fs::remove_file(&env_file);
            return Err(format!("容器任务 env 文件写失败：{e}"));
        }
        // ASKPASS 助手脚本：有 SCM 凭据时挂载（git checkout 在容器内读凭据；
        // svn 走 --password-from-stdin 不用 ASKPASS，挂载亦无害）。
        let askpass = match scm {
            Some(_) => {
                let p = temp_path(&spec.job_id, spec.attempt, "askpass.sh");
                if let Err(e) = write_askpass(&p) {
                    let _ = std::fs::remove_file(&p);
                    let _ = std::fs::remove_file(&env_file);
                    return Err(format!("容器任务 ASKPASS 脚本写失败：{e}"));
                }
                Some(p)
            }
            None => None,
        };
        // HOME 重定向目录（容器内 /sisyphus/workspace/.sisyphus-home）：宿主侧
        // 工作区里预建（容器以同 uid 运行可写；跨步骤持久、随工作区清理回收）。
        let _ = std::fs::create_dir_all(ws_dir.join(".sisyphus-home"));

        Ok(Self {
            image,
            env_file,
            askpass,
            ws_host: ws_dir.to_path_buf(),
            user: agent_uid_gid(),
            job_id: spec.job_id.clone(),
            attempt: spec.attempt,
            labels: vec![
                (LABEL_MANAGED_KEY.into(), LABEL_MANAGED_VAL.into()),
                (LABEL_JOB_KEY.into(), spec.job_id.clone()),
            ],
            docker_bin: DOCKER_BIN.to_string(),
        })
    }

    /// 容器名 `sisyphus-<job>-<attempt>-<stepseq>-<短随机>`（经纯函数
    /// [`container_name`] 装配，单测覆盖命名格式 + 清洗）。
    fn container_name_for_step(&self, step_seq: i32) -> String {
        container_name(&self.job_id, self.attempt, step_seq, &short_suffix())
    }

    /// 装配一条 `docker run` 的 [`RunSpec`] + 容器名（per-step）。shell 步骤与
    /// checkout 子命令共用——只差 `workdir` 与 `command`（Feature Envy 收口：
    /// [`ContainerSpawner`] 不再直接读 8 个 `ContainerTask` 字段）。
    fn run_spec(&self, workdir: String, command: Vec<String>, step_seq: i32) -> (RunSpec, String) {
        let name = self.container_name_for_step(step_seq);
        let spec = RunSpec {
            image: self.image.clone(),
            env_file: self.env_file.clone(),
            workspace_host: self.ws_host.clone(),
            workdir,
            user: self.user,
            askpass_host: self.askpass.clone(),
            name: name.clone(),
            labels: self.labels.clone(),
            command,
        };
        (spec, name)
    }

    /// 起一个 docker CLI 子进程（cwd = 系统 temp、env 空——机密只经 env 文件，
    /// 不进 docker CLI 的 `Command::env`）。[`spawn_run`] 与 [`pull`] 共用。
    fn spawn_docker(
        &self,
        args: Vec<String>,
        pipe_stdin: bool,
    ) -> Result<crate::exec::SpawnedStep, SpawnError> {
        crate::exec::spawn_command(
            &self.docker_bin,
            &args,
            &std::env::temp_dir(),
            &HashMap::new(),
            pipe_stdin,
        )
    }

    /// 装配 + 起一个 `docker run` 进程（[`run_spec`] + [`assemble_run`] +
    /// [`spawn_docker`]）。返回 (进程句柄, 容器名——取消/超时补刀用)。
    fn spawn_run(
        &self,
        spec: RunSpec,
        pipe_stdin: bool,
    ) -> Result<(crate::exec::SpawnedStep, String), SpawnError> {
        let name = spec.name.clone();
        let spawned = self.spawn_docker(assemble_run(&spec), pipe_stdin)?;
        Ok((spawned, name))
    }

    /// 起一个 shell 步骤容器：`docker run ... /bin/sh -c <command>`，`-w` 固定
    /// `/sisyphus/workspace`。返回 (进程句柄, 容器名——取消/超时补刀用)。env 经
    /// env 文件递送（不进 docker CLI 的 `Command::env`，机密只在文件）。
    pub(crate) fn spawn_shell(
        &self,
        command: &str,
        step_seq: i32,
    ) -> Result<(crate::exec::SpawnedStep, String), SpawnError> {
        let (spec, _) = self.run_spec(
            WORKSPACE_MOUNT_TARGET.into(),
            vec!["/bin/sh".into(), "-c".into(), command.into()],
            step_seq,
        );
        self.spawn_run(spec, false)
    }

    /// 执行一个 checkout 步骤（容器内）：复用 [`checkout::plan`] 规划子命令
    /// （plan_ws = 容器内 `/sisyphus/workspace`）、[`checkout::run_planned`]
    /// 执行循环（[`ContainerSpawner`] 包成 `docker run`）。`cred_env` 空（凭据经
    /// env 文件 + ASKPASS 挂载递送）；`need_init` 在宿主工作区上判（`.git`/`.svn`）。
    /// 返回 `(终态, 可选容器名)`——取消/超时携带在跑容器名供 [`Self::rm_f`] 补刀。
    //
    // 参数多于 clippy 阈值：step/host_ws/credential 是 checkout 输入、余下是 step
    // 上下文——与 [`crate::checkout::run`] 同款 allow。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_checkout(
        &self,
        step: &CheckoutStep,
        host_ws: &Path,
        credential: Option<&ScmCredential>,
        secrets: Vec<Vec<u8>>,
        trunc: Arc<Truncation>,
        job_id: &str,
        attempt: i32,
        cancel_rx: watch::Receiver<bool>,
        deadline: Option<Instant>,
        logbuf: &LogBuffer,
        step_seq: i32,
    ) -> (JobOutcome, Option<String>) {
        let vcs = match checkout::vcs_of(step) {
            Some(v) => v,
            None => {
                return (
                    JobOutcome::SpawnFailed("checkout 失败：未知 VcsType".into()),
                    None,
                );
            }
        };
        let need_init = match vcs {
            VcsType::VcsGit => !host_ws.join(".git").exists(),
            VcsType::VcsSvn => !host_ws.join(".svn").exists(),
        };
        // plan_ws = 容器内路径：git/svn 子命令在容器内操作 /sisyphus/workspace
        // （挂载源是宿主工作区，文件系统状态经挂载落盘）。cred_env 空：凭据经 env
        // 文件 + ASKPASS 挂载递送，不进子命令 args/env。
        let plan_ws = Path::new(WORKSPACE_MOUNT_TARGET);
        let cmds = match checkout::plan(
            step,
            plan_ws,
            need_init,
            &checkout::ScmBins::default(),
            &[],
            credential,
        ) {
            Ok(p) => p,
            Err(e) => return (JobOutcome::SpawnFailed(format!("checkout 失败：{e}")), None),
        };
        let spawner = ContainerSpawner {
            task: self,
            step_seq,
        };
        checkout::run_planned(
            &cmds, secrets, trunc, job_id, attempt, cancel_rx, deadline, logbuf, &spawner,
        )
        .await
    }

    /// 任务首步前显式 `docker pull`（always，ADR-0018）。输出流式进日志；失败 =
    /// [`JobOutcome::SpawnFailed`]（detail 含镜像 + 私仓登录提示——条件性措辞，
    /// 不假设 401，ADR-0018「401 类提示」语义）；取消/超时映射对应终态。pull 不
    /// 创建容器，无需补刀。
    pub(crate) async fn pull(
        &self,
        trunc: Arc<Truncation>,
        cancel_rx: watch::Receiver<bool>,
        deadline: Option<Instant>,
        logbuf: &LogBuffer,
        job_id: &str,
        attempt: i32,
    ) -> JobOutcome {
        let args = vec!["pull".to_string(), self.image.clone()];
        let spawned = match self.spawn_docker(args, false) {
            Ok(s) => s,
            Err(SpawnError(e)) => {
                return JobOutcome::SpawnFailed(format!("docker pull spawn 失败：{e}"));
            }
        };
        let pull_timeout = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
        let outcome = crate::stepio::run_streamed_step(
            spawned,
            None,
            Vec::new(),
            trunc,
            job_id,
            attempt,
            pull_timeout,
            cancel_rx,
            logbuf,
        )
        .await;
        match outcome {
            crate::exec::StepOutcome::Exited(0) => JobOutcome::Succeeded,
            crate::exec::StepOutcome::Exited(code) => JobOutcome::SpawnFailed(format!(
                "docker pull {image} 失败（退出码 {code}）；若为私有仓库请确认 Agent 宿主机已 docker login",
                image = self.image
            )),
            crate::exec::StepOutcome::Cancelled => JobOutcome::Cancelled,
            crate::exec::StepOutcome::Timeout => JobOutcome::Timeout,
        }
    }

    /// 取消/超时补刀：`docker rm -f <name>`（幂等——No such container 报错忽略）。
    pub(crate) async fn rm_f(&self, name: &str) {
        let _ = rm_container(&self.docker_bin, name).await;
    }
}

impl Drop for ContainerTask {
    fn drop(&mut self) {
        // 任务毕即删（ADR-0018）：env 文件（含机密）+ ASKPASS 脚本。best-effort
        // （不存在/删失败忽略——临时文件不复用）。
        let _ = std::fs::remove_file(&self.env_file);
        if let Some(p) = &self.askpass {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 容器 checkout 子命令执行器：把 git/svn 子命令包成
/// `docker run ... <program> <args>`。`-w` 取子命令规划 cwd（容器内路径）；
/// env 经 env 文件递送（忽略 `env` 参数——机密只在文件，不上命令行/CLI env）。
/// 返回 `(进程, Some(容器名))`——取消/超时供 [`crate::runner`] 补刀。
struct ContainerSpawner<'a> {
    task: &'a ContainerTask,
    step_seq: i32,
}

impl<'a> checkout::CommandSpawner for ContainerSpawner<'a> {
    fn spawn(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        _env: &HashMap<String, String>,
        pipe_stdin: bool,
    ) -> Result<(crate::exec::SpawnedStep, Option<String>), SpawnError> {
        let mut command = Vec::with_capacity(args.len() + 1);
        command.push(program.to_string());
        command.extend(args.iter().cloned());
        // run_spec + spawn_run：RunSpec 装配收口在 ContainerTask（Feature Envy
        // 修复），spawner 不再直接读 task 字段。
        let (spec, _) =
            self.task
                .run_spec(cwd.to_string_lossy().into_owned(), command, self.step_seq);
        let (spawned, name) = self.task.spawn_run(spec, pipe_stdin)?;
        Ok((spawned, Some(name)))
    }
}

/// Agent 自身 uid:gid（Linux 宿主：映射进容器，避免容器内 root 在挂载工作区落盘
/// 卡死宿主侧缓存 save/工作区清理，ADR-0018/0011）。其它平台 `None`（macOS/Windows
/// as-is，Docker Desktop 类 Linux 引擎自行翻译）。
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // libc::getuid/getgid 为 extern fn（edition 2024 调用 unsafe）
fn agent_uid_gid() -> Option<(u32, u32)> {
    unsafe { Some((libc::getuid(), libc::getgid())) }
}

#[cfg(not(target_os = "linux"))]
fn agent_uid_gid() -> Option<(u32, u32)> {
    None
}

/// 临时文件路径：系统 temp + `sisyphus-container-<job>-<attempt>.<ext>`。
/// 同 job 去重由 runner 保证（不并发），跨 job 唯一（job_id 唯一）。
fn temp_path(job_id: &str, attempt: i32, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "sisyphus-container-{}-{attempt}.{ext}",
        sanitize_name(job_id)
    ))
}

// ============================================================
// 单元测试（纯装配 + env 文件 + 命名 + 清扫 best-effort；无 daemon 即绿）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn run_spec(askpass: Option<PathBuf>, user: Option<(u32, u32)>) -> RunSpec {
        RunSpec {
            image: "alpine:3.20".into(),
            env_file: PathBuf::from("/tmp/sisyphus-env-job1.env"),
            workspace_host: PathBuf::from("/srv/ws/pipe/job"),
            workdir: WORKSPACE_MOUNT_TARGET.into(),
            user,
            askpass_host: askpass,
            name: "sisyphus-job1-0-0-1a2b3c4d".into(),
            labels: vec![
                (LABEL_MANAGED_KEY.into(), LABEL_MANAGED_VAL.into()),
                (LABEL_JOB_KEY.into(), "job1".into()),
            ],
            command: vec!["/bin/sh".into(), "-c".into(), "echo hello".into()],
        }
    }

    /// args 中是否存在某 flag+值对（flag 与值是相邻两个 args）。
    fn has_pair(args: &[String], flag: &str, val: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == val)
    }

    /// args 中是否包含某 flag。
    fn has(args: &[String], s: &str) -> bool {
        args.iter().any(|a| a == s)
    }

    #[test]
    fn assemble_run_includes_fixed_mount_workdir_envfile_name_labels_entry_command() {
        let spec = run_spec(None, Some((1000, 1000)));
        let args = assemble_run(&spec);
        // docker run --rm 开头。
        assert_eq!(args[0], "run");
        assert!(has(&args, "--rm"), "--rm 自清（正常路径）");
        // 工作区挂载 → /sisyphus/workspace。
        assert!(
            has_pair(
                &args,
                "-v",
                &format!("{}:{WORKSPACE_MOUNT_TARGET}", spec.workspace_host.display())
            ),
            "工作区挂载到 {WORKSPACE_MOUNT_TARGET}：{args:?}"
        );
        // 工作目录 -w /sisyphus/workspace。
        assert!(
            has_pair(&args, "-w", WORKSPACE_MOUNT_TARGET),
            "-w 工作目录：{args:?}"
        );
        // --user 1000:1000。
        assert!(
            has_pair(&args, "--user", "1000:1000"),
            "--user uid:gid：{args:?}"
        );
        // --env-file。
        assert!(
            has_pair(&args, "--env-file", &spec.env_file.to_string_lossy()),
            "--env-file：{args:?}"
        );
        // --name + 归属 labels。
        assert!(has_pair(&args, "--name", &spec.name), "--name：{args:?}");
        assert!(
            has_pair(&args, "--label", LABEL_MANAGED),
            "managed label：{args:?}"
        );
        assert!(
            has_pair(&args, "--label", "sisyphus.job=job1"),
            "job label：{args:?}"
        );
        // 镜像 + 入口/参数在末尾。
        let image_idx = args.iter().position(|a| a == "alpine:3.20").expect("image");
        assert_eq!(args[image_idx + 1], "/bin/sh", "入口 /bin/sh");
        assert_eq!(args[image_idx + 2], "-c");
        assert_eq!(args[image_idx + 3], "echo hello", "命令字符串作为最后 arg");
        // 无 ASKPASS 挂载（askpass_host = None）。
        assert!(
            !args.iter().any(|a| a.contains(ASKPASS_MOUNT_TARGET)),
            "无 SCM 凭据 → 不挂载 ASKPASS：{args:?}"
        );
    }

    #[test]
    fn assemble_run_omits_user_when_none() {
        let spec = run_spec(None, None);
        let args = assemble_run(&spec);
        assert!(
            !has(&args, "--user"),
            "无 user（macOS/Windows as-is）：{args:?}"
        );
    }

    #[test]
    fn assemble_run_includes_askpass_mount_when_present() {
        let spec = run_spec(Some(PathBuf::from("/tmp/sisyphus-askpass-job1.sh")), None);
        let args = assemble_run(&spec);
        assert!(
            has_pair(
                &args,
                "-v",
                &format!(
                    "{}:{ASKPASS_MOUNT_TARGET}:ro",
                    "/tmp/sisyphus-askpass-job1.sh"
                )
            ),
            "有 SCM 凭据 → 只读挂载 ASKPASS 到 {ASKPASS_MOUNT_TARGET}：{args:?}"
        );
    }

    #[test]
    fn container_name_format_sanitizes_and_prefixes() {
        let name = container_name("job-1_abc.2", 0, 3, "1a2b3c4d");
        assert_eq!(name, "sisyphus-job-1_abc.2-0-3-1a2b3c4d");
        // 非法字符（空格/斜杠/冒号）→ _。
        let dirty = container_name("job 1/2:3", 1, 0, "abcd1234");
        assert_eq!(
            dirty, "sisyphus-job_1_2_3-1-0-abcd1234",
            "非法字符→_：{dirty}"
        );
        // 空串 job_id → _。
        let empty = container_name("", 0, 0, "abcd1234");
        assert_eq!(empty, "sisyphus-_-0-0-abcd1234");
        // 首字符为字母（容器名要求）。
        assert!(name.starts_with("sisyphus-"));
    }

    #[test]
    fn short_suffix_is_hex_and_unique_across_calls() {
        let a = short_suffix();
        let b = short_suffix();
        assert_eq!(a.len(), 8, "8 位 hex：{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "hex 字符：{a}");
        assert_ne!(a, b, "连续调用应不同（计数器递增）");
    }

    #[test]
    fn write_env_file_writes_key_value_and_redirects_home() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("env");
        let mut env = HashMap::new();
        env.insert("MY_VAR".into(), "hello".into());
        env.insert("HOME".into(), "/users/should-be-dropped".into());
        env.insert("EMPTY".into(), "".into());
        write_env_file(&path, &env, None).expect("写 env 文件");
        let content = std::fs::read_to_string(&path).expect("读回");
        assert!(content.contains("MY_VAR=hello"), "任务 env 写入：{content}");
        assert!(content.contains("EMPTY="), "空值 env 写入：{content}");
        // HOME 重定向（用户 HOME 丢弃）。
        assert!(
            !content.contains("/users/should-be-dropped"),
            "用户 HOME 丢弃：{content}"
        );
        assert!(
            content.contains(&format!("HOME={HOME_IN_CONTAINER}")),
            "HOME 重定向到 .sisyphus-home：{content}"
        );
        // 末尾换行。
        assert!(content.ends_with('\n'), "末尾换行");
    }

    #[test]
    fn write_env_file_includes_scm_env_when_credential() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("env");
        let env = HashMap::new();
        let cred = ScmCredential {
            username: "alice".into(),
            password: "hunter2-secret".into(),
        };
        write_env_file(&path, &env, Some(&cred)).expect("写");
        let content = std::fs::read_to_string(&path).expect("读回");
        assert!(
            content.contains("GIT_ASKPASS=/sisyphus/askpass.sh"),
            "ASKPASS env：{content}"
        );
        assert!(
            content.contains("SISY_SCM_USER=alice"),
            "username env：{content}"
        );
        assert!(
            content.contains("SISY_SCM_PASS=hunter2-secret"),
            "password env：{content}"
        );
        assert!(
            content.contains("GIT_TERMINAL_PROMPT=0"),
            "禁终端提示：{content}"
        );
    }

    #[test]
    fn write_env_file_omits_scm_env_when_no_credential() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("env");
        let env = HashMap::new();
        write_env_file(&path, &env, None).expect("写");
        let content = std::fs::read_to_string(&path).expect("读回");
        assert!(
            !content.contains("GIT_ASKPASS"),
            "无凭据 → 无 ASKPASS env：{content}"
        );
        assert!(
            !content.contains("SISY_SCM_PASS"),
            "无凭据 → 无密码 env：{content}"
        );
    }

    #[test]
    fn askpass_script_has_shebang_and_no_credentials() {
        let script = askpass_script();
        assert!(script.starts_with("#!/bin/sh"), "shebang 使容器内可 invoke");
        assert!(script.contains("SISY_SCM_USER"), "读 env 取用户名");
        assert!(script.contains("SISY_SCM_PASS"), "读 env 取密码");
        // 静态、不含任何具体凭据字面量。
        assert!(!script.contains("password="));
        assert!(!script.contains("alice"));
    }

    #[test]
    fn write_askpass_creates_executable_script() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("askpass.sh");
        write_askpass(&path).expect("写 ASKPASS");
        assert!(path.exists(), "文件已创建");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), askpass_script());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "ASKPASS 0700（容器内 git 可 invoke）：{mode:o}"
            );
        }
    }

    /// 无 docker 时启动清扫 best-effort 跳过（不报错、不 rm）。
    #[tokio::test]
    async fn cleanup_orphan_containers_missing_binary_is_ok() {
        // 不存在的 docker 二进制 → list spawn 失败 → unwrap_or_default → 空 → Ok。
        let res = cleanup_orphan_containers("sisyphus-no-such-docker-zzz").await;
        assert!(
            res.is_ok(),
            "无 docker 应 best-effort Ok（不报错）：{res:?}"
        );
    }
}

//! shell 步骤的宿主机进程执行（ADR-0002/0006/0008；票 B3-T5 / #59）。
//!
//! 本模块只管「进程怎么起、怎么停」——spawn shell 步骤或裸命令（checkout
//! 的 git/svn 子命令，票 B3-T6 / #60）、stdout/stderr/stdin 流句柄交调用方
//! 编码、进程树终止（取消/超时）、超时与取消的竞争回收。步骤编排（JobSpec
//! → ack → 步骤序贯 → 终态）、日志事件编码、机密脱敏、`${SISY_WORKSPACE}`
//! 替换归 [`crate::runner`]；本模块不碰 proto/logbuf。
//!
//! - **默认解释器**（ADR-0006）：Unix `sh -c`；Windows `pwsh -NoProfile -Command`，
//!   pwsh 不可用则 `cmd /C`。命令字符串作为单 argv 传入（不经 shell 二次解析
//!   拼接——sh -c / pwsh -Command / cmd /C 自行解析其内）。
//! - **cwd / env**：cwd = 任务工作区；env = 继承 Agent 宿主环境 + JobSpec.env
//!   覆盖（ADR-0002「宿主机直跑共享构建机环境」；PATH 等继承是子进程能找到
//!   `sh`/`pwsh`/工具的前提）。
//! - **进程树终止**（ADR-0008）：Unix 子进程以 `process_group(0)` 成自身进程
//!   组长（pgid = pid），`kill(-pgid, SIGKILL)` 杀整组（含后台子进程）；
//!   Windows `taskkill /T /F /PID` 杀整树。
//! - **超时/取消竞争**：[`SpawnedStep::wait_until`] 用 `tokio::pin!` 钉住
//!   `child.wait()` future，在 `wait` 与 `cancel`/`sleep(timeout)` 间 select；
//!   命中取消/超时即 [`kill_tree`]（仅凭 pid，不触 `&mut child` 借用冲突）再
//!   reap 钉住的 wait future（避免「wait 被并发调用」的脚枪——同一 future
//!   实例重 poll，非二次 wait）。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::watch;

/// 进程起停错误（spawn 失败；其余由 [`StepOutcome`] 表达）。
#[derive(Debug)]
pub struct SpawnError(pub String);

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SpawnError {}

/// 步骤执行终态（终态语义归 runner 映射到 JobPhase；本模块只报进程层面结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// 自然退出，携带退出码（Unix 信号死亡取 -1）。
    Exited(i32),
    /// 被 CancelBuild 信号触发 → 进程树终止。
    Cancelled,
    /// 超 job 级超时 → 进程树终止。
    Timeout,
}

/// 已起的 shell 步骤：持有 child 与树终止用的 pid（stdout/stderr 经
/// [`SpawnedStep::take_streams`] 交 runner 编码）。
pub struct SpawnedStep {
    child: Child,
    pid: u32,
}

/// 以默认解释器起一个 shell 步骤。`command` 须已做 `${SISY_WORKSPACE}` 替换
/// （runner 职责）。env 继承宿主 + `spec_env` 覆盖。stdin 不接管（shell 步骤
/// 不需要）。
pub fn spawn_shell(
    command: &str,
    cwd: &Path,
    spec_env: &HashMap<String, String>,
) -> Result<SpawnedStep, SpawnError> {
    let shell = default_shell();
    let mut args = shell.args.clone();
    args.push(command.to_string());
    spawn(&shell.program, &args, cwd, spec_env, false)
}

/// 起一个裸命令（checkout 的 git/svn 子命令，票 B3-T6）：`program` + `args`
/// 作为 argv 直传（不经 shell 二次解析——凭据绝不上命令行，ADR-0015/0016）。
/// env 继承宿主 + `env` 覆盖；`pipe_stdin=true` 时接管 stdin（svn
/// `--password-from-stdin` 经 stdin 递送密码，调用方经 [`SpawnedStep::take_stdin`]
/// 取句柄写入）。缺二进制（`NotFound`）→ [`SpawnError`] 携「缺 X 二进制」清晰
/// 报错，不静默降级（ADR-0016）。
pub fn spawn_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    pipe_stdin: bool,
) -> Result<SpawnedStep, SpawnError> {
    spawn(program, args, cwd, env, pipe_stdin)
}

/// 共享 spawn：argv 直传 + stdout/stderr piped（+ 可选 stdin piped）+ Unix
/// 独立进程组（整组终止前提）。缺二进制给清晰报错。
fn spawn(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    pipe_stdin: bool,
) -> Result<SpawnedStep, SpawnError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if pipe_stdin {
        cmd.stdin(Stdio::piped());
    }
    // Unix：子进程独立进程组（pgid = pid），整组终止的必要前提。Windows
    // 树终止走 taskkill /T（按父子关系，不依赖进程组），无需此设置。
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| {
        let msg = if e.kind() == std::io::ErrorKind::NotFound {
            format!("缺 {program} 二进制（PATH 未找到）")
        } else {
            format!("spawn {program} 失败：{e}")
        };
        SpawnError(msg)
    })?;
    let pid = child.id().unwrap_or(0);
    Ok(SpawnedStep { child, pid })
}

impl SpawnedStep {
    /// 取走 stdin 句柄（svn `--password-from-stdin`：调用方写入密码后关闭）。
    /// 须在 [`Self::wait_until`] 之前调用。
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// 取走 stdout/stderr 句柄（runner 起读流任务编码日志）。须在
    /// [`Self::wait_until`] 之前调用——wait 不需要流句柄。
    pub fn take_streams(&mut self) -> (Option<ChildStdout>, Option<ChildStderr>) {
        (self.child.stdout.take(), self.child.stderr.take())
    }

    /// 子进程 pid（树终止用；Unix 进程组 pgid = pid）。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 等待步骤结束，与取消/超时竞争：
    /// - 自然退出 → [`StepOutcome::Exited`]（退出码）。
    /// - `cancel` 触发 → [`kill_tree`] + reap → [`StepOutcome::Cancelled`]。
    /// - `timeout` 到点 → [`kill_tree`] + reap → [`StepOutcome::Timeout`]。
    ///
    /// `timeout = None` = 无限等待（仅与 cancel 竞争）。`cancel` 是电平触发
    /// （`watch::Receiver<bool>`，true = 取消）：进入 wait 时已置位则立即
    /// 返回 Cancelled（覆盖「取消在步骤间到达、本步刚起」的窗口）。
    pub async fn wait_until(
        &mut self,
        timeout: Option<Duration>,
        mut cancel: watch::Receiver<bool>,
    ) -> StepOutcome {
        let pid = self.pid;
        // 钉住 wait future：select 各分支以 `&mut wait_fut` 共用同一 future 实例
        // （非二次 wait）。
        let wait_fut = self.child.wait();
        tokio::pin!(wait_fut);
        // 先经 select 裁决触发源（自然退出即直接返回）；取消/超时分支的 handler
        // 不 await——选出触发源后 cancel/sleep future 即 drop（其 `watch::Ref`
        // 非 Send，不跨 await 持有），随后 kill_tree + reap 只持 wait_fut（Send）。
        let triggered = match timeout {
            Some(t) => tokio::select! {
                s = &mut wait_fut => return exit_outcome(s),
                _ = cancel.wait_for(|c| *c) => Trigger::Cancel,
                _ = tokio::time::sleep(t) => Trigger::Timeout,
            },
            None => tokio::select! {
                s = &mut wait_fut => return exit_outcome(s),
                _ = cancel.wait_for(|c| *c) => Trigger::Cancel,
            },
        };
        kill_tree(pid);
        let _ = (&mut wait_fut).await; // reap 被杀的子进程
        match triggered {
            Trigger::Cancel => StepOutcome::Cancelled,
            Trigger::Timeout => StepOutcome::Timeout,
        }
    }
}

/// 取消/超时触发源（自然退出在 select 内直接 return，不经此枚举）。
enum Trigger {
    Cancel,
    Timeout,
}

/// 退出态 → [`StepOutcome::Exited`]（Unix 信号死亡 code=None 取 -1）。
fn exit_outcome(s: std::io::Result<std::process::ExitStatus>) -> StepOutcome {
    match s {
        Ok(st) => StepOutcome::Exited(st.code().unwrap_or(-1)),
        // wait 失败（极少）按非零退出处理，终态归 runner 裁决。
        Err(_) => StepOutcome::Exited(-1),
    }
}

// ============================================================
// 默认解释器选择
// ============================================================

/// 解释器程序 + 命令前导参数（命令字符串作为最后一个 arg 传入）。
struct Shell {
    program: String,
    args: Vec<String>,
}

/// 平台默认解释器（ADR-0006）：Unix `sh -c`；Windows pwsh 无则 cmd。
fn default_shell() -> Shell {
    #[cfg(unix)]
    {
        Shell {
            program: "sh".into(),
            args: vec!["-c".into()],
        }
    }
    #[cfg(windows)]
    {
        select_windows_shell(pwsh_available())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Shell {
            program: "sh".into(),
            args: vec!["-c".into()],
        }
    }
}

/// Windows 解释器选择（纯函数：`pwsh` 可用性由 `pwsh` 探针注入，便于单测
/// 两条分支而无需真实增删 pwsh）。
#[cfg(windows)]
fn select_windows_shell(pwsh: bool) -> Shell {
    if pwsh {
        Shell {
            program: "pwsh".into(),
            args: vec!["-NoProfile".into(), "-Command".into()],
        }
    } else {
        Shell {
            program: "cmd".into(),
            args: vec!["/C".into()],
        }
    }
}

/// 探针 pwsh 是否可用（spawn `pwsh --version` 成功即认为在 PATH 中）。失败
/// 回退 cmd。
#[cfg(windows)]
fn pwsh_available() -> bool {
    std::process::Command::new("pwsh")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

// ============================================================
// 进程树终止
// ============================================================

/// Unix：进程组 SIGKILL。子进程以 `process_group(0)` 成自身组长（pgid=pid），
/// `kill(-pgid, SIGKILL)` 杀整组——含 `sh -c 'sleep 30 &'` 类后台子进程
/// （未 setsid 的子进程继承父进程组）。ESRCH（组已不存在）忽略。
#[cfg(unix)]
#[allow(unsafe_code)] // libc::kill 标记 unsafe；FFI 无内部数据访问
fn kill_tree(pid: u32) {
    // 负 pid = 进程组号；SIGKILL 杀整组。
    let _ = unsafe { libc::kill(-(pid as pid_t), libc::SIGKILL) };
}

/// Windows：`taskkill /T /F /PID` 杀整树（/T = 含子进程，/F = 强制）。
#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 其他平台（当前发布矩阵之外）：无树终止能力，no-op。
#[cfg(not(any(unix, windows)))]
fn kill_tree(_pid: u32) {}

/// Unix pid 类型别名（libc::pid_t）。
#[cfg(unix)]
#[allow(non_camel_case_types)] // FFI 类型别名沿用 C 名 pid_t
type pid_t = libc::pid_t;

// ============================================================
// 单元测试（解释器选择纯函数 + spawn/wait 真实闭环）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_shell_prefers_pwsh_then_cmd() {
        let pwsh = select_windows_shell(true);
        assert_eq!(pwsh.program, "pwsh");
        assert!(pwsh.args.iter().any(|a| a == "-Command"));
        let cmd = select_windows_shell(false);
        assert_eq!(cmd.program, "cmd");
        assert!(cmd.args.iter().any(|a| a == "/C"));
    }

    /// spawn → wait 真实闭环：`echo` 退出码 0、stdout 可读。跨平台用各自
    /// 默认解释器能跑的命令（Unix `sh -c 'true'`、Windows pwsh/cmd `echo`）。
    #[tokio::test]
    async fn spawn_shell_runs_and_exits_zero() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let (program, args, command) = if cfg!(unix) {
            ("sh", vec!["-c".to_string()], "true".to_string())
        } else {
            // Windows：默认解释器（pwsh 或 cmd）都能跑的命令。
            ("cmd", vec!["/C".to_string()], "exit 0".to_string())
        };
        // 直接用默认解释器 spawn（不经 default_shell 以便跨平台断言）。
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args).arg(&command).current_dir(dir.path());
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().expect("spawn");
        let status = child.wait().await.expect("wait");
        assert!(status.success(), "true/exit 0 应成功退出");
    }

    /// spawn_command 起裸命令：argv 直传、stdout 可读、退出 0。跨平台用
    /// `git --version`（CI 与开发机均装 git；票 B3-T6 checkout 子命令的 spawn 通道）。
    #[tokio::test]
    async fn spawn_command_runs_raw_argv_and_exits_zero() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let env = HashMap::new();
        let args = vec!["--version".to_string()];
        let mut spawned = spawn_command("git", &args, dir.path(), &env, false).expect("spawn git");
        assert!(spawned.pid() > 0, "应捕到 pid");
        // 取走 stdout（证明 argv 直传的 git 真的跑了：--version 输出可读）。
        let (stdout, _stderr) = spawned.take_streams();
        let _ = spawned.take_stdin();
        let mut out = stdout.expect("stdout piped");
        let mut buf = Vec::new();
        use tokio::io::AsyncReadExt;
        let _ = out.read_to_end(&mut buf).await;
        let status = spawned.child.wait().await.expect("wait");
        assert!(status.success(), "git --version 应成功退出");
        assert!(
            String::from_utf8_lossy(&buf).contains("git version"),
            "stdout 应含 git 版本输出：{}",
            String::from_utf8_lossy(&buf)
        );
    }

    /// 缺二进制清晰报错（ADR-0016：不静默降级）：spawn_command 指向 PATH 中
    /// 不存在的程序 → SpawnError 消息点名「缺 … 二进制」，不 panic。
    #[tokio::test]
    async fn spawn_command_missing_binary_reports_clear_error() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let env = HashMap::new();
        let args: Vec<String> = vec![];
        let err = match spawn_command(
            "sisyphus-no-such-binary-zzz",
            &args,
            dir.path(),
            &env,
            false,
        ) {
            Ok(_) => panic!("缺二进制应报错，却 spawn 成功"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("缺"), "消息应点名缺失：{msg}");
        assert!(
            msg.contains("sisyphus-no-such-binary-zzz"),
            "消息应含程序名：{msg}"
        );
    }

    /// pipe_stdin=true 时 take_stdin 返回句柄（svn --password-from-stdin 通道）：
    /// 写入并关闭后进程读到 EOF 退出。用 `git hash-object --stdin`（两平台均装
    /// git）读 stdin 算哈希——写完关闭后退出 0 且 stdout 产出 40 位 hex 哈希，
    /// 证明 stdin 句柄可用且数据真的递了进去。
    #[tokio::test]
    async fn spawn_command_pipes_stdin_when_requested() {
        let dir = tempfile::tempdir().expect("临时工作区");
        // git hash-object 需仓库上下文：先 init（非测试焦点，仅为提供上下文）。
        let _ = std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let env = HashMap::new();
        let args = vec!["hash-object".to_string(), "--stdin".to_string()];
        let mut spawned = spawn_command("git", &args, dir.path(), &env, true).expect("spawn");
        let mut stdin = spawned
            .take_stdin()
            .expect("pipe_stdin=true 应给 stdin 句柄");
        let (stdout, _stderr) = spawned.take_streams();
        let mut stdout = stdout.expect("stdout piped");
        // 后台写 stdin + 关闭；同时读 stdout 到 EOF。
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let write = tokio::spawn(async move {
            let _ = stdin.write_all(b"hello\n").await;
            let _ = stdin.shutdown().await;
        });
        let mut out_buf = Vec::new();
        let _ = stdout.read_to_end(&mut out_buf).await;
        let status = spawned.child.wait().await.expect("wait");
        let _ = write.await;
        assert!(
            status.success(),
            "git hash-object --stdin 读 stdin 后应退出 0"
        );
        let hash = String::from_utf8_lossy(&out_buf);
        assert_eq!(
            hash.trim().len(),
            40,
            "应产出 40 位 hex 哈希（证明 stdin 数据递达）：{hash}"
        );
    }

    /// pipe_stdin=false 时 take_stdin 返回 None（shell 步骤与不需 stdin 的 checkout
    /// 子命令的默认）。spawn 后立即 take 验证。
    #[tokio::test]
    async fn spawn_command_no_stdin_when_not_requested() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let env = HashMap::new();
        let args = vec!["--version".to_string()];
        let mut spawned = spawn_command("git", &args, dir.path(), &env, false).expect("spawn");
        assert!(
            spawned.take_stdin().is_none(),
            "pipe_stdin=false 应无 stdin 句柄"
        );
        // reap 子进程（不读 stdout：--version 输出 << 64KiB 管道缓冲，不阻塞）。
        let _ = spawned.take_streams();
        let _ = spawned.child.wait().await;
    }

    /// 取消触发进程树终止：一个会挂起的长睡眠被 cancel 信号杀掉，wait_until
    /// 返回 Cancelled 且 promptly（远短于 sleep 时长）。
    #[tokio::test]
    async fn cancel_kills_long_running_step() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let command = if cfg!(unix) {
            "sleep 30".to_string()
        } else {
            // Windows：ping 127.0.0.1 是常见「睡几秒」的可移植招（ping -n 30）。
            "ping -n 30 127.0.0.1".to_string()
        };
        let env = HashMap::new();
        let mut step = spawn_shell(&command, dir.path(), &env).expect("spawn");
        let _ = step.take_streams();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let wait = tokio::spawn(async move { step.wait_until(None, cancel_rx).await });

        // 给进程起跑时间，再取消。
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel_tx.send(true).expect("取消信号");
        let started = std::time::Instant::now();
        let outcome = wait.await.expect("join");
        assert_eq!(outcome, StepOutcome::Cancelled);
        // 应在秒级内回收（被杀而非自然睡满 30s）。
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "取消应 promptly 终止进程树，实际 {:?}",
            started.elapsed()
        );
    }

    /// 超时触发进程树终止：长睡眠超短超时 → Timeout。
    #[tokio::test]
    async fn timeout_kills_long_running_step() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let command = if cfg!(unix) {
            "sleep 30".to_string()
        } else {
            "ping -n 30 127.0.0.1".to_string()
        };
        let env = HashMap::new();
        let mut step = spawn_shell(&command, dir.path(), &env).expect("spawn");
        let _ = step.take_streams();
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let started = std::time::Instant::now();
        let outcome = step
            .wait_until(Some(Duration::from_millis(300)), cancel_rx)
            .await;
        assert_eq!(outcome, StepOutcome::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时应 promptly 终止进程树"
        );
    }

    /// 进入 wait 时取消信号已置位 → 立即 Cancelled（覆盖「取消在步骤间到达、
    /// 本步刚起」窗口，不睡满 sleep）。
    #[tokio::test]
    async fn pre_set_cancel_returns_immediately() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let command = if cfg!(unix) {
            "sleep 30".to_string()
        } else {
            "ping -n 30 127.0.0.1".to_string()
        };
        let env = HashMap::new();
        let mut step = spawn_shell(&command, dir.path(), &env).expect("spawn");
        let _ = step.take_streams();
        let (_cancel_tx, cancel_rx) = watch::channel(true); // 已置位
        let started = std::time::Instant::now();
        let outcome = step.wait_until(None, cancel_rx).await;
        assert_eq!(outcome, StepOutcome::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "已置位的取消应立即返回"
        );
    }

    /// Unix 进程组终止：后台子进程随主进程被整组杀掉（证明树终止而非仅杀
    /// 直接子进程）。Windows 不适用（taskkill /T 已含子进程，无需进程组证明）。
    #[cfg(unix)]
    #[tokio::test]
    async fn tree_kill_kills_backgrounded_child() {
        let dir = tempfile::tempdir().expect("临时工作区");
        let pidfile = dir.path().join("child.pid");
        // `; wait` 让 sh 阻塞在后台 sleep 上（而非命令一结束就退出），
        // 否则 child.wait() 瞬间完成走自然退出分支、不进 cancel→kill_tree，
        // cancel_tx.send 会因对端已 drop 而报 SendError，后台 sleep 成孤儿。
        let command = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let env = HashMap::new();
        let mut step = spawn_shell(&command, dir.path(), &env).expect("spawn");
        let _ = step.take_streams();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // 等主进程把后台子进程 pid 落盘。
        let pidfile_clone = pidfile.clone();
        let wait = tokio::spawn(async move { step.wait_until(None, cancel_rx).await });
        let child_pid: u32 = {
            let mut got = None;
            for _ in 0..50 {
                if let Ok(text) = std::fs::read_to_string(&pidfile_clone)
                    && let Ok(p) = text.trim().parse::<u32>()
                {
                    got = Some(p);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            got.expect("应捕到后台子进程 pid")
        };

        cancel_tx.send(true).expect("取消");
        let outcome = wait.await.expect("join");
        assert_eq!(outcome, StepOutcome::Cancelled);

        // 进程组被杀后，后台 sleep 子进程应已不存在（kill -0 失败）。
        // 给 OS 一点回收时间。
        let mut gone = false;
        for _ in 0..40 {
            let status = std::process::Command::new("kill")
                .arg("-0")
                .arg(child_pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !status.map(|s| s.success()).unwrap_or(false) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "后台子进程 {child_pid} 应随进程组终止消失");
    }
}

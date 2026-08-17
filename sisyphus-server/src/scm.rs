//! scm 模块：SCM 集成层 trait 缝（ADR-0016，票 B2c-T6）。
//!
//! poll 触发源探测经本 trait 缝隔离：trigger 模块消费 [`ScmProbe::probe_head`]
//! 拿到项目默认分支 / HEAD 的当前提交标识（git head sha / svn HEAD
//! revision），用以基线 / 去重 / 触发。真实 `git ls-remote` / `svn info`
//! 探测随 scm 批次落地——本批只立 trait 缝 + 假探测（[`FakeProbe`]，可控
//! head / 失败序列）验证基线 / 节奏 / 去重 / 历史逻辑，生产面暂挂
//! [`UnimplementedProbe`]（poll 记 `last_probe_error`、按节奏重试、不自动
//! 禁用），scm 批次换入真实探测即可（main 装配一处）。
//!
//! cron 触发源不经探测（默认值 + 默认分支 head：触发上下文钉默认分支、
//! 不钉 commit，Agent 执行期检分支头），故本 trait 仅服务 poll。

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::store::projects::Project;

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
}

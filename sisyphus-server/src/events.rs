//! 进程内事件总线（票 #46，ADR-0009：只做热通知、可丢、DB 重放兜底）。
//!
//! SSE/notify 等热消费方订阅本总线；消息可丢无害——终态读路径永远从
//! SQLite 重放（ADR-0005 重放兜底），bus 只是「有东西变了」的通知管线。
//! 事件类型为进程管线 enum（build/job/agent 三类），不进 sisyphus-model
//! （model 是跨 crate 契约，事件是 server 内部管线）。
//!
//! 容量有限（64）的 broadcast 通道：满队时最老消息被丢、慢接收者收
//! `Lagged`——这正是「可丢热通知」语义，消费方不可依赖每条必达。

use tokio::sync::broadcast;

use crate::store::builds::{BuildStatus, TriggerSource};
use crate::store::jobs::JobStatus;

/// 进程管线事件（BuildStatus 终态是 notify 的挂接点；JobStatus 供任务视图
/// 热刷新；Agent 上下线供 sched/UI 在线态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 构建入队（`start_build` 统一入口产出）。
    BuildCreated {
        /// 构建行 id。
        build_id: i64,
        /// 属主项目名。
        project_name: String,
        /// pipeline 名。
        pipeline_name: String,
        /// per-pipeline 自增构建号。
        number: i64,
        /// 触发源。
        trigger: TriggerSource,
    },
    /// 构建状态迁移（queued→running、终态等）。
    BuildStatus {
        /// 构建行 id。
        build_id: i64,
        /// 属主项目名。
        project_name: String,
        /// pipeline 名。
        pipeline_name: String,
        /// per-pipeline 自增构建号。
        number: i64,
        /// 新状态。
        status: BuildStatus,
        /// 重跑 attempt。
        attempt: i32,
    },
    /// 任务状态迁移（入池/跳过/失败/取消/重试等）。
    JobStatus {
        /// 任务行 id。
        job_id: i64,
        /// 属主构建 id。
        build_id: i64,
        /// 阶段序号。
        stage_index: i32,
        /// 任务名。
        name: String,
        /// 新状态。
        status: JobStatus,
        /// 第几次执行。
        attempt: i32,
    },
    /// Agent 上线（心跳恢复在线置位）。
    AgentOnline {
        /// Agent 行 id。
        agent_id: i64,
        /// 构建机名。
        name: String,
    },
    /// Agent 离线（45s 无心跳判离线）。
    AgentOffline {
        /// Agent 行 id。
        agent_id: i64,
        /// 构建机名。
        name: String,
    },
    /// 日志 chunk 落库（票 #73，ADR-0013）：SSE 尾随的热通知。可丢——
    /// 消费方收到后从 DB 重放（游标即 seq），Lagged 自愈；丢消息无害。
    LogAppended {
        /// 属主构建 id。
        build_id: i64,
        /// 任务行 id。
        job_id: i64,
        /// 第几次执行。
        attempt: i32,
    },
}

/// 进程内事件总线：`publish` 广播给全部订阅者，无订阅者/满队即丢。
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

/// 通道容量：热通知不需要背压堆积，64 足够覆盖一批状态迁移的突发。
const BUS_CAPACITY: usize = 64;

impl EventBus {
    /// 新建总线（无订阅者）。
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// 订阅事件流（每订阅者独立游标；慢消费超容量丢中间消息——可丢语义）。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// 广播一条事件。无订阅者（send 报错）与慢消费（Lagged）都直接忽略
    /// ——DB 是真相源，热通知丢了无妨。
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_reaches_subscribers_in_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Event::AgentOnline {
            agent_id: 1,
            name: "linux-1".into(),
        });
        bus.publish(Event::AgentOffline {
            agent_id: 1,
            name: "linux-1".into(),
        });
        assert_eq!(
            rx.try_recv().expect("首条"),
            Event::AgentOnline {
                agent_id: 1,
                name: "linux-1".into(),
            }
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::AgentOffline { agent_id: 1, .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    /// 可丢语义：无订阅者时 publish 不报错（send 的 Err 被吞）。
    #[test]
    fn publish_with_no_subscribers_is_silently_dropped() {
        let bus = EventBus::new();
        bus.publish(Event::BuildCreated {
            build_id: 1,
            project_name: "demo".into(),
            pipeline_name: "release".into(),
            number: 1,
            trigger: TriggerSource::Manual,
        });
        // 不 panic 即通过；再订阅也收不到（已丢）。
        assert!(bus.subscribe().try_recv().is_err());
    }

    /// 慢消费丢中间消息（Lagged）——可丢热通知的边界，消费方靠 DB 重放。
    #[tokio::test]
    async fn slow_consumer_lags_and_drops_oldest() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        // 不消费、把通道灌满（容量 64）。
        for i in 0..BUS_CAPACITY {
            bus.publish(Event::AgentOnline {
                agent_id: i as i64,
                name: format!("a-{i}"),
            });
        }
        bus.publish(Event::AgentOnline {
            agent_id: 999,
            name: "a-999".into(),
        });
        assert!(
            matches!(
                rx.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
            ),
            "慢消费应收 Lagged（丢中间消息）"
        );
        // Lagged 后游标落在「最老保留消息」而非最新——逐条读过去直到收到
        // 最新一条（999），证明丢中间消息后仍可继续消费。
        let mut last: i64 = -1;
        while last != 999 {
            match rx.recv().await {
                Ok(Event::AgentOnline { agent_id, .. }) => last = agent_id,
                other => panic!("收到非 AgentOnline 或通道关闭：{other:?}"),
            }
        }
    }
}

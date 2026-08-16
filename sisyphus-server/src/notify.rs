//! 构建终态通知钩子（票 #46，ADR-0006：pipeline 完成时发送，失败必发、
//! 成功可配）。本批只留订阅点不发送：SMTP 发送实现随 notify 批次，这里
//! 只把「终态事件」接出来挂个位——`spawn_notifier` 是 notify 批次的接线
//! 点，届时在钩子里读构建快照的 `notification` 配置并调用发送器。

use crate::events::{Event, EventBus};

/// 是否为值得挂接通知的终态构建事件（失败必发、成功可配——成功与否由
/// 通知批次读快照配置裁决，这里只过滤「终态」这一事实）。
pub fn is_notifiable_terminal(event: &Event) -> bool {
    matches!(
        event,
        Event::BuildStatus { status, .. } if status.is_terminal()
    )
}

/// 订阅事件总线、挂接终态钩子的后台任务。本批不发送任何通知，只留位
/// （trace 记录终态事件，notify 批次在此接 SMTP）。
pub fn spawn_notifier(bus: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(event) if is_notifiable_terminal(&event) => {
                    // 留位：notify 批次在此取构建快照 notification 配置并发送
                    //（失败必发、成功可配，ADR-0006）。
                    let Event::BuildStatus { build_id, number, status, .. } = event else {
                        unreachable!("is_notifiable_terminal 已过滤非 BuildStatus");
                    };
                    tracing::trace!(build_id, number, status = status.as_str(), "构建终态事件（通知挂接点，本批不发送）");
                }
                // 可丢热通知：Lagged/Closed 直接忽略、继续收。
                Ok(_) | Err(_) => continue,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::store::builds::BuildStatus;

    fn terminal_event(status: BuildStatus) -> Event {
        Event::BuildStatus {
            build_id: 1,
            project_name: "demo".into(),
            pipeline_name: "release".into(),
            number: 1,
            status,
            attempt: 1,
        }
    }

    #[test]
    fn terminal_filter_keeps_only_build_terminal_status() {
        for status in [
            BuildStatus::Succeeded,
            BuildStatus::Failed,
            BuildStatus::Cancelled,
            BuildStatus::Timeout,
        ] {
            assert!(is_notifiable_terminal(&terminal_event(status)), "{status:?} 为终态");
        }
        assert!(!is_notifiable_terminal(&terminal_event(BuildStatus::Running)));
        assert!(!is_notifiable_terminal(&terminal_event(BuildStatus::Queued)));
        assert!(!is_notifiable_terminal(&Event::JobStatus {
            job_id: 1,
            build_id: 1,
            stage_index: 0,
            name: "compile".into(),
            status: crate::store::jobs::JobStatus::Succeeded,
            attempt: 1,
        }));
    }
}

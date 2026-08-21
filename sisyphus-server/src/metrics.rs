//! 指标 facade（ADR-0019，票 B5-T7）：`metrics` crate 埋点 + Prometheus 文本
//! 渲染，同一份计数喂两路——内部快照（REST 概览端点）+ `/metrics` 端点。
//!
//! - **安装**（[`install`]）：进程内全局 recorder 只装一次（`metrics` crate
//!   全局 recorder 单次语义），由组合根装配点调用（[`crate::api::AppState::new`]）。
//!   选 `metrics-exporter-prometheus` 的 [`PrometheusBuilder::build_recorder`]——
//!   进程内 recorder + 按需 `handle().render()`，不 spawn 独立 HTTP 端口
//!   （ADR-0019：`/metrics` 与业务路由同端口，鉴权与业务同缝）。
//! - **双消费**：
//!   - 事件型指标（构建终态计数、构建时长直方图、gRPC 流断连计数、调度
//!     活动时间）在事件点直接埋点（[`record_build_terminal`]、
//!     [`record_grpc_disconnect`]、[`touch_scheduler`]）；
//!   - 当前值型指标（队列深度/Agent/槽位/磁盘占用）由概览快照计算函数
//!     [`crate::snapshot::compute`] 落真值（DB 真相源），经 [`report_snapshot`]
//!     灌入 recorder（同一份数即快照响应的值）——`/metrics` 读同一 recorder。
//!     快照端点每被调一次灌一次；调度循环周期 tick 也灌一次（[`sched`]
//!     周期面），保 `/metrics` 无 UI 轮询时也新鲜。
//! - **命名**：Prometheus 最佳实践（recorder 侧 `with_recommended_naming` 已
//!   开启 counter `_total` 后缀 + 单位后缀；counter 常量名自带 `_total`，
//!   exporter 不会重复追加）。标签（队列深度原因分类、构建终态结果、gRPC
//!   断连原因）为低基数固定值。
//!
//! 未安装 recorder 时（纯库使用、无组合根的早期路径）`metrics` crate 落到
//! noop recorder——埋点零成本，`/metrics` 端点不存在，不炸。

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// 全局安装守卫（`metrics` crate 全局 recorder 只能装一次；Once 保证进程内
/// 恰好一次 build + set，且 [`HANDLE`] 永远指向真正装上的 recorder）。
static ONCE: std::sync::Once = std::sync::Once::new();
/// 全局 recorder 句柄（`/metrics` 端点渲染用；安装后持有一生）。
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// 调度队列深度（当前值 gauge；`reason` 标签 = 无匹配原因分类）。
pub const QUEUE_DEPTH: &str = "sisyphus_queue_depth";
/// Agent 在线数（当前值 gauge）。
pub const AGENTS_ONLINE: &str = "sisyphus_agents_online";
/// Agent 总数（当前值 gauge）。
pub const AGENTS_TOTAL: &str = "sisyphus_agents_total";
/// 槽位占用（当前值 gauge；running/unknown 在途任务总数）。
pub const SLOTS_USED: &str = "sisyphus_slots_used";
/// 槽位总量（当前值 gauge；全部在线 Agent 的 max_concurrency 之和）。
pub const SLOTS_TOTAL: &str = "sisyphus_slots_total";
/// 构建终态计数（counter；`result` 标签 = succeeded/failed/cancelled/timeout）。
pub const BUILDS_TERMINAL: &str = "sisyphus_builds_terminal_total";
/// 构建时长（直方图；秒，从 queued→running 到终态）。
pub const BUILD_DURATION_SECONDS: &str = "sisyphus_build_duration_seconds";
/// 产物 + 日志磁盘占用字节（当前值 gauge；`kind` 标签 = artifacts/logs）。
pub const STORAGE_BYTES: &str = "sisyphus_storage_bytes";
/// gRPC 流断连计数（counter；`reason` 标签 = handshake_fail/disconnect/read_error/disabled）。
pub const GRPC_DISCONNECTS: &str = "sisyphus_grpc_disconnects_total";
/// 调度循环最后活动时间（gauge；Unix 毫秒。ADR-0019：只进 /metrics 不进
/// healthz——healthz 是给编排器的二值信号）。
pub const SCHEDULER_LAST_ACTIVITY: &str = "sisyphus_scheduler_last_activity_ms";

/// 安装全局 metrics recorder（幂等：进程内只装一次，后续调用返回既有句柄。
/// 由 [`crate::api::AppState::new`] 在组合根装配点调用——测试多 AppState 共享
/// 同一 recorder，埋点跨测试继续累积，属预期：prometheus 是进程级计数面）。
pub fn install() -> PrometheusHandle {
    ONCE.call_once(|| {
        // default-features=false 的 PrometheusBuilder：build_recorder 不要求
        // tokio 运行时、不 spawn 后台任务——纯 recorder + 按需 render（本仓库
        // 指标全是低基数固定标签，无 unbounded 风险）。
        // 构建时长直方图设 bucket（秒，量级覆盖秒级~小时级构建）；不设则
        // exporter 以 summary 渲染（quantile），与「直方图」语义不符。
        let recorder = PrometheusBuilder::new()
            .with_recommended_naming(true)
            .set_buckets(&[
                1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
            ])
            .expect("bucket 列表非空")
            .build_recorder();
        let handle = recorder.handle();
        // 全局 recorder 只装一次：若被并发/他处先装（Err），以已装者为准——
        // 本仓库是唯一安装者，正常路径必成功。
        let _ = metrics::set_global_recorder(recorder);
        let _ = HANDLE.set(handle);
    });
    HANDLE.get().expect("Once 内已设置句柄").clone()
}

/// `/metrics` 文本渲染：`metrics-exporter-prometheus` 的 Prometheus 文本格式
/// （含 `# HELP`/`# TYPE`）。recorder 未安装时返回空串（早期路径/未装配）。
pub fn render() -> String {
    HANDLE.get().map_or_else(String::new, |h| h.render())
}

/// 构建终态计数 + 时长直方图（engine/sched 构建终态点埋点）。
/// `result` 为固定标签值（`succeeded`/`failed`/`cancelled`/`timeout`）；
/// `duration_ms` 为终态时刻 - started_at（无 started_at 的不记时长——排队中
/// 直接取消的构建无从计时）。
pub fn record_build_terminal(result: &'static str, duration_ms: i64) {
    metrics::counter!(BUILDS_TERMINAL, "result" => result).increment(1);
    if duration_ms > 0 {
        metrics::histogram!(BUILD_DURATION_SECONDS).record(duration_ms as f64 / 1000.0);
    }
}

/// gRPC 流断连计数（`session_loop` 各断开臂埋点：对端关流 / 读帧失败 /
/// 停用踢线）。`reason` 为固定标签值。
pub fn record_grpc_disconnect(reason: &'static str) {
    metrics::counter!(GRPC_DISCONNECTS, "reason" => reason).increment(1);
}

/// 调度循环最后活动时间（秒级周期 tick 埋点；Unix 毫秒）。
pub fn touch_scheduler(now_ms: i64) {
    metrics::gauge!(SCHEDULER_LAST_ACTIVITY).set(now_ms as f64);
}

/// 概览快照当前值灌入 recorder（同一份数喂 `/metrics`；快照端点 + 调度周期
/// 面共用）。输入即 [`crate::snapshot::compute`] 的返回。
///
/// 队列深度按原因分类（reason 标签固定低基数）：**固定标签全集始终输出**——
/// 空队列也把每个原因置 0（`/metrics` 输出稳定，Prometheus 不缺维度）。
pub fn report_snapshot(s: &crate::snapshot::Snapshot) {
    // 固定原因标签全集（与 snapshot::classify 的标签值一一对应）。
    for reason in [
        "no_online_agent",
        "missing_labels",
        "no_slot",
        "uncategorized",
    ] {
        let depth = s.queue.get(reason).copied().unwrap_or(0);
        metrics::gauge!(QUEUE_DEPTH, "reason" => reason).set(depth as f64);
    }
    metrics::gauge!(AGENTS_ONLINE).set(s.agents_online as f64);
    metrics::gauge!(AGENTS_TOTAL).set(s.agents_total as f64);
    metrics::gauge!(SLOTS_USED).set(s.slots_used as f64);
    metrics::gauge!(SLOTS_TOTAL).set(s.slots_total as f64);
    metrics::gauge!(STORAGE_BYTES, "kind" => "artifacts").set(s.artifact_bytes as f64);
    metrics::gauge!(STORAGE_BYTES, "kind" => "logs").set(s.log_bytes as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 安装后渲染出 Prometheus 文本；七项指标 + 调度活动时间的契约名出现。
    /// 先经 [`report_snapshot`] + 事件埋点注册全部指标（recorder 按需注册——
    /// 从未埋点的指标不会凭空出现在输出；真实服务器由调度周期面每轮灌入）。
    #[test]
    fn install_and_render_contains_metric_names() {
        let _h = install();
        report_snapshot(&crate::snapshot::Snapshot {
            queue: std::collections::BTreeMap::new(),
            agents_online: 0,
            agents_total: 0,
            slots_used: 0,
            slots_total: 0,
            builds_terminal: std::collections::BTreeMap::new(),
            artifact_bytes: 0,
            log_bytes: 0,
            has_no_match: false,
            has_offline_agent: false,
            has_draining_incompatible: false,
        });
        record_build_terminal("succeeded", 0);
        record_grpc_disconnect("handshake_fail");
        touch_scheduler(1);

        let text = render();
        assert!(!text.is_empty(), "已安装 recorder 应渲染出文本");
        for name in [
            QUEUE_DEPTH,
            AGENTS_ONLINE,
            AGENTS_TOTAL,
            SLOTS_USED,
            SLOTS_TOTAL,
            BUILDS_TERMINAL,
            BUILD_DURATION_SECONDS,
            STORAGE_BYTES,
            GRPC_DISCONNECTS,
            SCHEDULER_LAST_ACTIVITY,
        ] {
            assert!(
                text.contains(name),
                "{name} 应出现在 /metrics 输出：\n{text}"
            );
        }
    }

    /// 事件型埋点后文本可见（构建终态计数 + 标签、调度活动时间）。
    #[test]
    fn event_metrics_render_with_values() {
        let _h = install();
        record_build_terminal("succeeded", 5_000);
        touch_scheduler(1_700_000_000_000);

        let text = render();
        assert!(
            text.contains("result=\"succeeded\""),
            "终态标签出现：\n{text}"
        );
        assert!(
            text.contains(SCHEDULER_LAST_ACTIVITY),
            "调度活动时间出现：\n{text}"
        );
    }
}

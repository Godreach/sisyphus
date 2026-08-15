//! sisyphus-server 库面：Server 全部模块与组合根（ADR-0009）。
//!
//! bin（`src/main.rs`）是薄壳：解析 CLI → 合并配置（ADR-0010）→ 初始化
//! tracing（ADR-0019）→ 经 [`api::router`] 与 [`grpc::service`] 组装
//! REST + gRPC 双服务并行 serve（ADR-0005 端口合并策略推迟，各自独立监听）。
//!
//! 集成测试与二进制走同一组合根：REST 端点经 `tower::ServiceExt::oneshot`
//! 进程内驱动 [`api::router`]，不起 socket、不 spawn 进程（Spec B2a）。

pub mod api;
pub mod config;
pub mod grpc;
pub mod store;

mod auth;
mod engine;
mod events;
mod notify;
mod sched;
mod scm;
mod trigger;

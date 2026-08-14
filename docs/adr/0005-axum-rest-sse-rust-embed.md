# 0005 - Server 用 axum，REST + utoipa，SSE 推日志，rust-embed 内嵌前端

日期：2026-08-15
状态：已接受

## 背景

Server 的 web 框架与 API 风格选型（调研报告：`research/rust-web-framework` 分支，issue #6）。约束：tokio 异步栈 + sqlx（ADR-0004）、Vue 3 SPA 前端（ADR-0003）、单二进制零依赖、单进程多职责（web UI + API + agent 通信 + 调度）。agent 通信协议是独立票（#4），本决策须对它的三种走向（gRPC/WebSocket/轮询）均不构成约束。

## 决策

- **Web 框架：axum 0.8**（tokio 团队官方维护，建在 hyper/tower 上）。
- **前端 API：REST（JSON over HTTP），`/api/v1/` 前缀从第一天做起**；OpenAPI 用 **utoipa 5**（`#[derive(ToSchema)]` 编译期生成，Swagger UI 挂进路由）。
- **实时推送：SSE**（日志流、任务/pipeline 状态广播走同一 SSE 通道；断线用 Last-Event-ID/offset 续传，日志落 SQLite 重放便宜）。
- **前端静态文件：rust-embed 8**（release 编译期嵌入、debug 运行时读盘）+ 自写 axum 静态 handler（SPA fallback：非 API 未命中路径回 index.html）；预留本地覆盖目录层（Gitea 分层资产模式）。
- WebSocket 保留给未来交互式终端场景（axum `ws` feature 即插即用），v1 不用。

## 理由

- **tokio 生态同源是决定性的**：axum/tonic/tower-http/tokio 共享同一套 Service 抽象，中间件全链路复用。tonic 0.14 的 `Routes` 内部就是 `axum::Router`（官方 Cargo.toml 依赖 `axum = "0.8"`，`into_axum_router` 双向转换）--若 #4 选 gRPC，可同端口原生共存。axum 累计下载 4.24 亿，是 Rust web 事实标准。
- **rocket 出局**（0.5.1 停在 2024-05，27 个月无稳定版）；actix-web 合格但与 tonic 组合需跨两套 Service 抽象，且无一等公民 SSE；poem 社区小一个数量级。
- **SSE 恰好匹配单向日志流**：EventSource 自动重连 + Last-Event-ID、纯 HTTP 无 upgrade、反代友好；Woodpecker 生产即用 SSE 推日志。

## 后果

- **axum 0.x 升级税**：0.8 已稳定一年+，0.9 在路上；项目未写代码，直接从 0.8 起步。tonic 与 axum 版本联动。
- **SSE 经反代需配置**（nginx 默认缓冲流，需 `X-Accel-Buffering: no`）：部署文档必写，这是 SSE 相对 WebSocket 仅存的部署侧代价。
- **utoipa 注解漂移风险**：CI 里跑 OpenAPI snapshot 测试兜底；aide（同构方案）作为远期备选。
- **rust-embed 仓库自托管**（非 GitHub）：API 面极小，include_dir 是 escape hatch。
- 对 #4 的输入：gRPC / WebSocket / 轮询三种走向 axum 均无约束，端口策略（同端口 merge vs 双端口）推迟到部署设计再定。

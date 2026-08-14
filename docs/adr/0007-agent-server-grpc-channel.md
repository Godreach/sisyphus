# 0007 - Agent 与 Server 通信：gRPC 双向流 + 一次性注册码 + 独立 HTTP 产物通道

日期：2026-08-15
状态：已接受

## 背景

Agent（全矩阵 Linux/macOS/Windows × x86_64/aarch64，构建机多在内网/NAT 后）与 Server（axum 0.8，ADR-0005）之间的通信协议。硬约束：连接方向只能 Agent 主动外连（NAT 穿越友好）；客户端库跨平台成熟度；~100 Agent、日千级构建规模。ADR-0005 明确对本票三种走向（gRPC/WebSocket/轮询）均不构成约束。盘问过程见 [wayfinder 票 #4](https://github.com/Godreach/sisyphus/issues/4)。

## 决策

- **主通道：Agent 主动外连 gRPC 双向流（tonic）**，与 axum 同进程原生融合（tonic 0.14 `Routes` ↔ `axum::Router`；同端口 merge 技术上免费，端口策略沿 ADR-0005 推迟到部署设计）。任务下发、心跳、日志、状态上报、取消全走此通道的多路复用流。
- **注册与认证**：管理员在 web UI 创建 Agent 条目并生成一次性注册码（单次失效、短有效期）→ Agent 以 `--server-url --reg-key` 启动，凭注册码换取长期 per-Agent token（Agent 侧落盘）。token 可单独吊销（UI 禁用 Agent 即踢线）。不做 mTLS，不做共享密钥开放注册。
- **传输加密**：TLS 可选（Server 可配证书），默认明文面向内网部署；文档明确 token 跨公网必须 TLS；Agent 侧提供自签证书跳过验证开关（打警告日志）。
- **心跳与离线判定**：在线状态 = 通道连接状态，应用层心跳 15s、45s 无心跳判离线；离线即停派新任务并上报调度层。无独立心跳端点。
- **日志与状态上报**：主通道独立 stream；日志块带 per-job 单调序列号（seq），Server 按 seq 落库；Agent 断线期间日志写本地磁盘缓冲，重连后按 seq 补传（不丢、不乱序）。任务状态变更同通道上报。
- **产物上传**：独立 HTTP 端点（axum REST 面，agent token 鉴权），大文件走 HTTP 分块/超时/限速语义。
- **协议演进**：proto 仅加字段，unknown field 忽略；Agent/Server 兼容版本窗口归发布工程。

## 理由

- **proto schema 先行是决定性的**：Agent/Server 版本偏差是长期现实，schema 演进（加字段、unknown field）免费获得，是 Agent 升级机制的地基。
- 一条 TCP 连接多路复用：任务控制与日志互不阻塞；~100 Agent 规模连接数可控。
- WebSocket 方案要自己发明消息 schema 与演进规则（JSON + 版本号），把 protobuf 已解决的问题重做一遍；且 ADR-0005 把 WS 留给未来交互式终端。
- 轮询的实时性（日志延迟 = 轮询间隔）不满足"实时日志流"的产品期待。
- 产物走 HTTP：gRPC message 默认 4MB 上限与流式背压对大文件不友好，HTTP 分块语义成熟。
- h2 过企业代理偶有坑是接受的成本：部署文档写明（内网 h2c 直连 / CONNECT 隧道）。

## 后果

- Agent 与 Server 各自面对两套 API 面：gRPC（agent 通道）+ REST（web 与产物上传）；utoipa 不覆盖 agent 面，proto 即契约。
- tonic 与 axum 版本联动（ADR-0005 已记）。
- 日志本地磁盘缓冲是 Agent 端必备组件（非可选优化）。
- 解锁下游：#5 调度票（离线事件输入）、#7 日志票（seq 块输入）、Agent 升级机制票（迷雾 graduate）。

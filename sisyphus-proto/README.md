# sisyphus-proto

Agent/Server 唯一共享契约：`.proto` 源文件 + tonic/prost 生成物（ADR-0007）。

## 定位

本 crate 是 Agent 与 Server 之间的**唯一共享契约**——两端都依赖它、再无别的共享面。契约先行、语言中立：`.proto` 即真相源，演进纪律为**只加字段、unknown field 忽略、N-1 兼容窗口**（ADR-0007）。生成物不进 git，构建期由 `build.rs` 现场生成。

## 契约概览

源文件位于 `proto/`（当前 `agent.proto`）。单一 gRPC 双向流 service `AgentChannel`，一条连接多路复用全部消息：

| 面向 | 消息 |
| --- | --- |
| 下行（Server → Agent） | `JobSpec`（任务下发）、`CancelBuild`（取消）、`UpgradeCommand`（升级）、`WorkspaceCommand`（工作区列表/清理）、`CacheCommand`（缓存列表/删除） |
| 上行（Agent → Server） | `Handshake`（握手）、`Heartbeat`（心跳 + 磁盘占用）、`JobAck`（回执）、`JobStatus`（状态）、`LogBatch`（日志流）、`JobReported`（在途任务上报）、`UpgradeStatus`、`WorkspaceList`、`CacheList` |

上行/下行统一收在 `ChannelMessage` 的 `oneof kind` 里，每帧一个变体。

## 模块结构

| 路径 | 说明 |
| --- | --- |
| `proto/agent.proto` | 契约源文件（`package sisyphus.v1`） |
| `build.rs` | 用 vendored protoc（`protoc-bin-vendored`）现场生成 tonic/prost 代码到 `OUT_DIR`；大 oneof（`JobSpec`）boxed 免 `large_enum_variant`；关 `build_transport` 避免 `connect(dst)` 与 RPC 方法名碰撞 |
| `src/lib.rs` | `pub mod agent`：`include!` 生成物 `sisyphus.v1.rs`（消息 + `AgentChannel` service）；`pub mod version` |
| `src/version.rs` | 本发行版本常量 `VERSION` 与兼容窗口判定（见下） |

生成物豁免 `missing_docs`（文档来自 `.proto` 注释，不手改）；workspace 的 `unsafe_code=warn` 豁免 `build.rs` 里 `set_var("PROTOC", …)`（1.97 起标记 unsafe，单线程 build.rs 无并发风险）。

## 版本与兼容窗口（ADR-0010/0017）

`version.rs` 把版本比较逻辑放在唯一共享 crate，两端复用、避免各自实现漂移：

- `VERSION`：当前发行版本（Server 与 Agent 同版本成对发布）。
- `compatible(peer, local)`：对端不高于本地即窗口内；`peer_too_new` 判定对端任意段大于本地——过新直接拒连。

## 构建

```bash
cargo build -p sisyphus-proto          # vendored protoc，无需系统 protoc（ADR-0009）
cargo test -p sisyphus-proto           # 含 version 兼容窗口单测
```

dev-deps 带 `tokio` / `tonic`：proto 缝的 fake Agent 闭环测试（真实 tonic 通道收 `JobSpec` → ack → 回状态）消费本 crate 生成的 client/server 面。

## 与其它 crate 的关系

- `sisyphus-server` 与 `sisyphus-agent` 都依赖本 crate；本 crate 不依赖任何其它 sisyphus crate（叶子）。
- 契约演进只动 `proto/*.proto` + 重生成；Agent/Server 的协议适配随各自批次落地。

## 参见

- [ADR-0007](../docs/adr/0007-agent-server-grpc-channel.md)（Agent/Server gRPC 通道）、[ADR-0008](../docs/adr/0008-scheduling-and-agent-routing.md)（调度与路由）、[ADR-0010](../docs/adr/0010-v1-release-form-and-installation-experience.md)（发布形态与兼容窗口）、[ADR-0017](../docs/adr/0017-agent-self-upgrade.md)（Agent 自升级）
- [顶层 README](../README.md)、[CONTEXT.md](../CONTEXT.md)（领域词汇表）

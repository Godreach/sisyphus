# sisyphus

自托管 CI 平台：单二进制 Server + 全平台 Agent，流水线定义存服务端、Web 可视化编辑。对标 Jenkins / 腾讯蓝盾（bk-ci）的使用形态。

## 定位

面向中小团队的持续集成平台，三个根本性取舍：

- **流水线定义只存 Server 端数据库**，通过 Web UI 可视化编辑——repo 内没有任何配置文件。这是与 Drone / Woodpecker（repo 内 YAML）的根本区别。
- **单进程 Server**：Web UI、REST API、编排调度在一个二进制里，不 master/worker 分离，不做高可用集群。v1 设计规模约 100 台 Agent、日千级构建。
- **Agent 全矩阵**：Linux / macOS / Windows × x86_64 / aarch64。默认在宿主机直接执行步骤（零依赖），每个任务可选容器执行环境（v1 以 Docker 为主）。

## 架构

```
浏览器 / 脚本（PAT）
   │  HTTP（REST + SSE）+ cookie 会话
   ▼
sisyphus-server（单进程、单二进制）
   │  engine 编排状态机 · sched 调度与 Agent 路由 · trigger（cron / poll SCM）
   │  auth · scm · notify · events（进程内总线）· store（SQLite + 产物存储，单 --data-dir）
   ▼
gRPC 通道（proto = Agent/Server 唯一共享契约；Server 下发已解析的 Job Spec）
   │
   ├─ sisyphus-agent @ Linux x86_64 / aarch64
   ├─ sisyphus-agent @ Windows x86_64
   └─ sisyphus-agent @ macOS x86_64 / aarch64
        宿主机直跑（默认）或容器后端 · 工作区 · 缓存 · 日志流式上报 · 产物上传
```

**技术栈**：Rust（axum：REST + SSE；tonic：gRPC 通道；sqlx + SQLite；rust-embed 内嵌前端），Vue 3 + VueFlow 前端，proto 作为 Agent/Server 间唯一共享契约。

### Workspace 布局

| 目录 | 说明 |
| --- | --- |
| `sisyphus-proto/` | `.proto` 源文件 + 生成物（tonic/prost），Agent/Server 唯一共享 crate |
| `sisyphus-model/` | 流水线定义 JSON 模型、when 表达式 AST、保存校验规则（纯逻辑叶子 crate） |
| `sisyphus-server/` | Server 二进制：api / engine / sched / trigger / scm / auth / store / events / notify |
| `sisyphus-agent/` | Agent 二进制：channel / runner / workspace / cache / upgrader，只依赖 proto |
| `sisyphus-web/` | Vue 3 前端 |
| `docs/adr/` | 架构决策记录（ADR） |

## 能力速览

- **编排**：阶段按序执行、阶段内任务并行；阶段/任务/步骤三级 when 条件；失败级联取消；从失败任务重跑（同号 attempt+1）或从头重跑（新构建号）。
- **触发**：手动、cron、poll SCM。
- **调度**：中心化全局匹配，Agent 标签 AND 全集匹配 + per-Agent 并发槽位。
- **执行**：宿主机直跑（默认）或容器；工作区跨构建复用、增量 checkout；跨构建缓存（key 模板 + LRU 淘汰）。
- **日志与产物**：任务级日志流式上报（断线缓冲补传）与 SSE 回放；产物任务级上传、构建内共享；构建快照永久可查。
- **多用户**：项目级 viewer/runner/admin 三档角色 + 全局管理员；会话 + 个人访问令牌（PAT）。

完整领域词汇表见 [CONTEXT.md](CONTEXT.md)。

## 发布与升级

- GitHub Releases：6 目标 × server/agent 分开压缩包 + sha256 校验和；官方 Docker 镜像仅 Server（双架构、非 root、`/data` 卷）。
- Server 与 Agent 同版本号成对发布；升级顺序 Server 先升，兼容窗口 N-1。
- 单一 `--data-dir`（内含 SQLite 与产物存储），首次启动生成带注释的 `config.toml`；启动自动前向迁移，迁移前自动备份 db。

## 项目状态

设计阶段：领域模型与架构决策已定稿（14 篇 ADR + [CONTEXT.md](CONTEXT.md) 词汇表，见 [docs/adr/](docs/adr/)），实现尚未开始。

## License

[MIT](LICENSE)

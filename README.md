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
- **机密存储的防护边界（ADR-0015）**：机密值以主密钥文件（`<data-dir>/master.key`，首启自动生成、0600，路径可经 config `[auth] master_key_path` 改到独立卷）+ XChaCha20-Poly1305 加密落库。该机制防「DB 文件/备份单独泄露」（库/备份脱离数据目录读不出明文）；**数据目录整体失守（含密钥文件）无解**，同机 root 亦不防——密钥文件须留在可信卷上。跨公网部署必须 TLS（v1 会话 cookie 不设 Secure，理由同上）。

## 开发

```bash
cargo build --workspace                        # 构建（vendored protoc，无需系统 protoc）
cargo test --workspace                         # 测试
cargo clippy --workspace -- -D warnings        # lint
```

### 数据库迁移工作流

迁移 SQL 位于 `sisyphus-server/src/store/migrations/`，经 `sqlx::migrate!` 编译期嵌入——单二进制自带迁移（ADR-0009）。新增迁移后：

1. 在 `migrations/` 加 `NNNN_描述.sql`，版本号递增、只加不破（proto 同款演进纪律）。
2. 若本次改动含编译期校验查询（`sqlx::query!` 宏），本地跑 `cargo sqlx prepare --workspace`（需 [sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli)，`DATABASE_URL` 指向已应用迁移的开发库），生成 `.sqlx/` 离线校验文件。
3. 连 `.sqlx/` 一起提交——CI 构建免 `DATABASE_URL`。

服务端启动时自动前向迁移，迁移前自动把 db（连 `-wal`/`-shm`）备份到 `<data-dir>/backups/`（ADR-0010）；不支持降级。

### REST 契约与 OpenAPI snapshot 守护

REST 端点由 utoipa 注解生成 OpenAPI 契约（ADR-0005），开发期（debug 构建）浏览 `/swagger-ui`（仅开发期挂载为 Spec B2a 裁定；release 不暴露）。契约快照 `sisyphus-server/tests/snapshots/openapi.json` 入 git：端点/形态漂移会被 snapshot 比对测试拦下。有意变更契约后：

```bash
UPDATE_SNAPSHOTS=1 cargo test -p sisyphus-server    # 重写快照
```

连快照一起提交——snapshot diff 即本次契约变更的评审面。

## 项目状态

实现进行中：B1 骨架（workspace 四 crate、proto 契约、model 保存校验、Agent 握手闭环、CI）与 Spec B2a 存储与 API 底座（配置合并、SQLite 池+迁移+备份、REST/gRPC 双服务、pipeline 定义读写闭环、内嵌静态资源）、Spec B2b 认证与用户体系（setup wizard、登录会话、登录限流、CSRF、PAT、三档角色、用户管理与注册开关）与项目机密服务端面（主密钥文件 + XChaCha20 加密落库 + 机密 CRUD：值只写不读）已落地——`/api/v1` 全面要求认证，未认证仅 login/setup/register 与静态资源可达。设计基线：21 篇 ADR + [CONTEXT.md](CONTEXT.md) 词汇表（见 [docs/adr/](docs/adr/)）。

## License

[MIT](LICENSE)

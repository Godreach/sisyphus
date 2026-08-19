# sisyphus-server

sisyphus Server 二进制：单进程承载 web UI、REST API、编排调度与 Agent 通道（ADR-0005/0009）。

## 定位

单进程、单二进制部署（v1 设计规模约 100 台 Agent、日千级构建）：Web UI、REST API、编排调度、Agent gRPC 通道都在一个进程里，不 master/worker 分离、不做高可用集群。lib + bin 结构（ADR-0009）——`src/main.rs` 是薄壳，全部模块实现在库面 `sisyphus_server`，集成测试与二进制走同一组合根。

## 启动路径（`src/main.rs`）

解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing（ADR-0019）→ 存储底座开池+迁移前备份+前向迁移（B2a-T2）→ 主密钥文件（ADR-0015）→ 装配 `AppState` → 调度循环 + 通知钩子 + 触发器装配 → 绑定双端口 → REST（axum）与 gRPC（tonic）并行 serve + 心跳超时扫描。任一端口被占即启动失败，不带病运行半个服务。

## 模块结构

顶层库模块（`src/lib.rs`）：

| 模块 | 说明 |
| --- | --- |
| `api` | REST API 组合根：router、`AppState`、业务端点、认证/CSRF/授权中间件 |
| `auth` | 认证域逻辑（纯逻辑可单测）：argon2id 密码、session id 原语、登录限流器、API token 基座、项目三档角色权限矩阵（ADR-0014） |
| `config` | 启动配置：CLI flag > `SISYPHUS_` env > config.toml > 内置默认（ADR-0010） |
| `engine` | 构建编排状态机：统一触发入口 `start_build`、推进 `drive`、任务终态 `on_job_terminal`、`ResolvedJobSpec` 组装（ADR-0006） |
| `events` | 进程内事件总线：热通知、可丢、DB 重放兜底（build/job/agent 三类，容量 64 broadcast） |
| `grpc` | Agent 通道（gRPC）：token 认证握手、停用即踢线、心跳在线判定（15s/45s）、任务面 JobSpec 下发/JobAck/JobStatus/JobReported/CancelBuild（ADR-0007/0008） |
| `notify` | 构建终态通知钩子（留位点，SMTP 发送随 notify 批次接） |
| `sched` | 调度与下发：事件驱动单循环、全局 FIFO pending 池、在线+空槽+标签 AND 匹配、per-Agent 槽位、job 超时、build 级取消、fail-fast 级联、Agent 离线 orphan 宽限（ADR-0008） |
| `scm` | SCM 集成 trait 缝：poll 触发源探测隔离（真实 `git ls-remote`/`svn info` 随 scm 批次换入） |
| `secrets` | 机密加密域逻辑（纯逻辑）：主密钥文件 + XChaCha20-Poly1305（ADR-0015） |
| `store` | SQLite 池 + PRAGMA + 编译期嵌入迁移（迁移前自动备份）+ repo 层（ADR-0004/0010） |
| `trigger` | cron + poll 触发源：cron 按表达式节奏扫表、poll 按项目节奏轮询（ADR-0016） |

### `api/` 子模块

| 文件 | 说明 |
| --- | --- |
| `mod` | router + `AppState` + 中间件装配；业务端点全挂 `/api/v1/`，统一 JSON 错误形态 |
| `auth` | 认证中间件（cookie 会话 + Bearer PAT 双通道，401） |
| `policy` | 授权 extractor（项目 viewer/runner/admin 三档 + 全局 admin，403/404） |
| `csrf` | CSRF 防护中间件（cookie 会话非安全方法，Bearer 免疫，403） |
| `error` | 统一 JSON 错误形态：code + message + detail |
| `docs` | OpenAPI 契约（utoipa 5，注解即契约）+ snapshot 守护 |
| `health` | `GET /healthz`（不鉴权不查库，Docker HEALTHCHECK 探活） |
| `web` | 静态资源：本地覆盖目录 → 内嵌产物（rust-embed） |
| `projects` / `pipelines` / `members` / `secrets` / `triggers` / `builds` / `agents` / `audit` / `users` / `tokens` | 各业务域端点 |

### `store/` 子模块

| 文件 | 说明 |
| --- | --- |
| `mod` | `bootstrap`：开池 + PRAGMA + 校验待应用迁移 + 备份 db（连 `-wal`/`-shm`）+ 前向迁移 |
| `projects` / `pipelines` / `builds` / `jobs` / `agents` / `triggers` | 元数据与调度状态 repo |
| `users` / `sessions` / `tokens` / `members` / `secrets` / `audit` | 认证与安全 repo |
| `traits` | `LogStore` / `ArtifactStore` 契约缝（只定形，随消费批次落实现） |

### `engine/` 子模块

| 文件 | 说明 |
| --- | --- |
| `mod` | 状态机：`start_build`（组装 `BuildSnapshot` 入队）、`drive`（FIFO 放行 → 阶段按序 → when 求值 → 任务并行下发 → 终态收集）、`on_job_terminal`（推进/自动重试/fail-fast 级联） |
| `spec` | `ResolvedJobSpec` 组装：变量替换、env 合并、机密按名解密注入、三级 when 求值过滤、隐式容器标签、SCM 上下文——proto `JobSpec` 的源 |

## 配置

- **优先级**：CLI flag > `SISYPHUS_` 前缀环境变量 > `config.toml` > 内置默认（ADR-0010）。
- **单一 `--data-dir`**（默认 `./data`）：内含 SQLite（`sisyphus.db`）、`artifacts/`、`backups/`、`web/`（静态资源本地覆盖）、`master.key`。首启生成带注释的 `config.toml`。
- **默认端口**：REST `0.0.0.0:8080`、Agent gRPC `127.0.0.1:50051`（各自独立监听，ADR-0005 端口合并策略推迟）。
- **日志**：默认 stdout JSON，可切 pretty；`RUST_LOG` 整体胜出（ADR-0019）。
- **注册开关**：默认关（账号由全局 admin 建）；**机密防护边界**见 ADR-0015（防 DB 单独泄露，数据目录整体失守无解）。

## 构建

```bash
cargo build -p sisyphus-server
cargo test -p sisyphus-server
cargo clippy -p sisyphus-server -- -D warnings
```

### 数据库迁移工作流

迁移 SQL 在 `src/store/migrations/`，经 `sqlx::migrate!` 编译期嵌入。新增迁移后若改了 `sqlx::query!` 宏，跑 `cargo sqlx prepare --workspace`（需 sqlx-cli + `DATABASE_URL`）生成 `.sqlx/` 离线校验文件，连 `.sqlx/` 一起提交——CI 构建免 `DATABASE_URL`。启动时自动前向迁移、迁移前自动备份，不支持降级。

### REST 契约与 OpenAPI snapshot 守护

端点由 utoipa 注解生成 OpenAPI；开发期（debug 构建）浏览 `/swagger-ui`（release 不暴露）。契约快照 `tests/snapshots/openapi.json` 入 git，漂移被 snapshot 比对测试拦下。有意变更契约后：

```bash
UPDATE_SNAPSHOTS=1 cargo test -p sisyphus-server    # 重写快照
```

### 集成测试纪律

REST 端点经 `tower::ServiceExt::oneshot` 进程内驱动 `api::router`，不起 socket、不 spawn 进程（Spec B2a）——测试与二进制共用同一装配。

## 与其它 crate 的关系

- 依赖 `sisyphus-proto`（gRPC 契约）+ `sisyphus-model`（定义模型与保存校验）。
- 经 gRPC 通道（proto）与 `sisyphus-agent` 交互：下发 `JobSpec`、收 `JobAck`/`JobStatus`/`JobReported`/日志流、下发 `CancelBuild`/升级/工作区/缓存指令。

## 参见

- [ADR-0004](../docs/adr/0004-sqlite-sqlx-local-artifacts.md)、[ADR-0005](../docs/adr/0005-axum-rest-sse-rust-embed.md)、[ADR-0006](../docs/adr/0006-pipeline-data-model-and-execution-semantics.md)、[ADR-0008](../docs/adr/0008-scheduling-and-agent-routing.md)、[ADR-0010](../docs/adr/0010-v1-release-form-and-installation-experience.md)、[ADR-0014](../docs/adr/0014-auth-and-user-model.md)、[ADR-0015](../docs/adr/0015-secrets-and-audit-log.md)、[ADR-0019](../docs/adr/0019-server-observability.md)
- [顶层 README](../README.md)、[CONTEXT.md](../CONTEXT.md)

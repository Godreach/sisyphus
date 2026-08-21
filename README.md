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

### 裸机安装（下载 + sha256 校验）

从 [Releases](https://github.com/Godreach/sisyphus/releases) 按目标下载对应包——命名 `sisyphus-{server,agent}-<ver>-<os>-<arch>.tar.gz`（Linux/macOS）/ `.zip`（Windows），受支持平台为 Linux x86_64/aarch64 + Windows x86_64，macOS 两架构 as-is 不承诺（ADR-0010 分级承诺）。

```bash
# 1. 下载 server 包 + sha256sums.txt（同 Release 附）
VER=1.0.0
curl -LO https://github.com/Godreach/sisyphus/releases/download/v${VER}/sisyphus-server-${VER}-linux-x86_64.tar.gz
curl -LO https://github.com/Godreach/sisyphus/releases/download/v${VER}/sha256sums.txt

# 2. 校验——只校本机目标包（-c 会对列出的每个文件查找，缺其余目标包会报错，
#    用 grep 过滤到本机目标即可）
grep "sisyphus-server-${VER}-linux-x86_64.tar.gz" sha256sums.txt | sha256sum -c -
# 预期输出：sisyphus-server-1.0.0-linux-x86_64.tar.gz: OK

# 3. 解压（得同级目录含可执行文件 + README + LICENSE）
tar xzf sisyphus-server-${VER}-linux-x86_64.tar.gz
cd sisyphus-server-${VER}-linux-x86_64
./sisyphus-server --data-dir ./data            # 首启生成 config.toml + master.key
```

Agent 同形：按构建机目标下载 `sisyphus-agent-<ver>-<os>-<arch>` 包，校验、解压后 `./sisyphus-agent --server-url https://… --registration-code XXXX` 注册并常驻（ADR-0010 Agent 首跑体验）。

### Docker 快速上手（仅 Server）

官方镜像 `ghcr.io/godreach/sisyphus-server`，双架构 manifest（`linux/amd64` + `linux/arm64`），debian-slim 基底，捆绑 git+subversion（SCM 探测零配置 ADR-0016），非 root 运行，`/data` 卷，`EXPOSE 8080`（REST）+ `50051`（gRPC），HEALTHCHECK 打 `/healthz`。

```bash
# 拉取（:latest 或 :<version>）
docker pull ghcr.io/godreach/sisyphus-server:latest

# 建首个管理员（headless，ADR-0010 / 票 #80；--password-stdin 从 stdin 读一行，
# 密码不出现在进程列表/shell history）
docker run --rm -i -v ./data:/data ghcr.io/godreach/sisyphus-server \
  admin create --password-stdin admin <<< 'your-admin-password'

# 常驻
docker run -d --name sisyphus-server -p 8080:8080 -v ./data:/data \
  ghcr.io/godreach/sisyphus-server:latest

# HEALTHCHECK 验证（Dockerfile 内置：30s 间隔探 REST /healthz）
docker inspect --format='{{.State.Health.Status}}' sisyphus-server   # healthy
docker ps --filter name=sisyphus-server --format '{{.Status}}'        # Up (healthy)
```

完整 Compose 示例见 [`examples/docker-compose.yml`](examples/docker-compose.yml)。

> **gRPC 端口注意（ADR-0005）**：Server 默认 bind REST `0.0.0.0:8080`（可直接发布）+ gRPC `127.0.0.1:50051`（**loopback**——容器内对远端 Agent 不可达）。远端 Agent 经 gRPC 连接时需覆盖 `SISYPHUS_GRPC_ADDR=0.0.0.0:50051` 并发布 `50051` 端口（见 compose 注释）；仅 REST + 本地 Agent 的部署可保持 50051 不发布。

### 升级顺序

ADR-0010 强制 **Server 先升**：Agent 版本新于 Server 时启动即拒连并明确报错。兼容窗口 **N-1**（1.x Server 支持上一个 minor 的 Agent；proto 仅加字段是技术基础）。升级 = 替换二进制重启：

1. **Server 先**：停服 → 替换 `sisyphus-server` 二进制 → 重启。启动自动跑前向迁移，**迁移前自动备份 db** 到 `<data-dir>/backups/`（单文件 SQLite 让备份成本趋近于零，弥补不支持降级的硬约束）。
2. **Agent 后**：逐台替换 `sisyphus-agent` 二进制重启（或经自升级机制 ADR-0017：管理员上传 agent 发行包 → Server 经 gRPC 下发升级指令 → Agent 自行下载/校验/换入/spawn）。

不支持降级。Agent 自升级失败退化为「手动替换二进制重启」这条原生兜底路径。

### 服务化

v1 仅文档示例（不做内置服务安装子命令，ADR-0010 边界）：

- **Linux**：systemd unit 模板 [`examples/sisyphus-server.service`](examples/sisyphus-server.service)。
- **Windows**：`sc.exe` / NSSM 指引 [`docs/service-windows.md`](docs/service-windows.md)（推荐 NSSM——把控制台 exe 包成服务）。

### v1 明确不做（ADR-0010 边界）

以下渠道 v1 不提供，等真实需求出现再评估：

- **安装脚本**（`curl | sh` 一键安装）——裸机安装走上文「下载 + sha256 校验」三步。
- **系统包**（deb / rpm / homebrew / choco）——不维护各发行版打包元数据。
- **cosign 签名**——release 产物仅 sha256 校验和，不附签名链。
- **agent 官方镜像**——与 ADR-0002「agent 默认宿主机直跑」相悖，agent 在构建机上裸跑。

### 首个管理员（headless 引导，ADR-0010 / 票 #80）

无浏览器/无 TTY 的 Docker 或 headless 部署，用 `admin create` 子命令建首个全局管理员（setup wizard 的 CLI 等价），跑过即视为引导完成（用户表非空 → web wizard 不再进入）。密码永不上 argv——经 stdin 读取：

```bash
# 二进制（解压 release 包后）
sisyphus-server --data-dir ./data admin create --password-stdin admin <<< 'your-admin-password'
# Docker（见上「Docker 快速上手」）
# 或交互 prompt（终端输入，v1 不回显留 follow-up）
sisyphus-server --data-dir ./data admin create admin
```

建号后再起 server 即可登录；再建管理员走 web 全局 admin 端点（`POST /api/v1/users`），`admin create` 仅建首个（用户表非空即拒）。

### 机密存储的防护边界（ADR-0015）

机密值以主密钥文件（`<data-dir>/master.key`，首启自动生成、0600，路径可经 config `[auth] master_key_path` 改到独立卷）+ XChaCha20-Poly1305 加密落库。该机制防「DB 文件/备份单独泄露」（库/备份脱离数据目录读不出明文）；**数据目录整体失守（含密钥文件）无解**，同机 root 亦不防——密钥文件须留在可信卷上。跨公网部署必须 TLS（v1 会话 cookie 不设 Secure，理由同上）。

## 开发

```bash
cargo build --workspace                        # 构建（vendored protoc，无需系统 protoc）
cargo test --workspace                         # 测试
cargo clippy --workspace -- -D warnings        # lint
```

### 提交信息

提交信息遵循 Conventional Commits——`<type>[(<scope>)][!]: <中文描述>`，type ∈ {feat,fix,docs,chore,style,refactor,perf,test,build,ci,revert}，scope 可选（如 `server`/`web`/`agent`）。feat/fix 末尾惯例带票号 `（票 #NN）`，body 末 `Closes #NN`。

```bash
# 正例
feat: 产物链路——存储/端点/Agent 传输/前端产物区（票 #74）
fix(web): headless 冒烟钉中文 locale——修 CI en-US 红灯
docs: 补五 crate README——proto/model/codegen/server/agent 各一份 crate 根文档
```

本地 commit-msg hook 与 CI 会拦下缺 `type:` 前缀的 subject（如 `产物链路：…`）。启用本地 hook（一次性，本地配置不入 git）：

```bash
git config core.hooksPath .githooks
```

完整规则与正例反例见 [docs/agents/commit-messages.md](docs/agents/commit-messages.md)。

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

### Agent 配置

优先级：CLI flag > `SISYPHUS_` 前缀环境变量 > 内置默认（与 Server 同纪律，ADR-0010；Agent 无 config.toml 层）。

| 配置项 | CLI flag | 环境变量 | 默认 |
| --- | --- | --- | --- |
| Server 地址（gRPC 通道） | `--server-url` | `SISYPHUS_SERVER_URL` | 无（缺则启动失败并打印缺参提示） |
| Server REST 基址（注册码兑 token / 产物 / 升级下载的 HTTP 面，与 gRPC 通道不同端口） | `--api-url` | `SISYPHUS_API_URL` | 无（仅 `--reg-key` 注册时需要） |
| 数据目录 | `--data-dir` | `SISYPHUS_DATA_DIR` | `~/.sisyphus-agent` |
| 日志级别 | `--log-level` | `SISYPHUS_LOG_LEVEL` | `info`（`RUST_LOG` 若设置则整体胜出） |
| 日志文件（追加 JSON） | `--log-file` | `SISYPHUS_LOG_FILE` | 无（日志走 stderr pretty） |
| 缓存容量上限（GiB，0 = 不限） | `--cache-capacity-gib` | `SISYPHUS_CACHE_CAPACITY_GIB` | 20（ADR-0012：per-Agent 容量上限，LRU 自动淘汰；磁盘容量是机器运维属性不参与调度） |

**首次接入（注册引导，票 #57）**：管理员在 web UI 建 Agent 条目并签发一次性注册码（24h 有效）后，构建机凭注册码兑长期 token 落盘再常驻：

```bash
sisyphus-agent --server-url http://<server>:50051 --api-url http://<server>:8080 --reg-key sisa_reg_xxx
```

注册成功 token 落 `<data>/token`（Unix 0600），后续启动读 token 直连、不再需要注册码；注册失败（无效/已用/过期/停用/网络不可达）明确报错退出。注册码一次性 + 24h 过期（ADR-0010）。

数据目录布局（票 B3-T1 五处约定）：根放 `token`（per-Agent 凭据，注册批次落盘）与 `agent.json`（本地状态）两个文件位，`workspaces/`（工作区根，ADR-0011）、`cache/`（缓存根 + registry.json，ADR-0012）、`logbuf/`（断线日志缓冲，ADR-0007/0013）三个子目录。心跳间隔、重连退避等运行参数内置默认（15s 心跳；重连 1s 起、×2、上限 60s、±20% 抖动、永久重试），不对外暴露。

## 贡献

环境前置、质量门、提交规范、PR 流程见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 项目状态

**实现已闭环，但未经运行验收，尚未进入 release 阶段**——22 篇 ADR 定义的设计已逐条落地，主线路径集成测试全绿，但功能尚未经人工运行与验收，未打 1.0.0 发布 tag。以下为分阶段落地概要（逐单元详情见 git 历史与各 crate README）：

- **B1 骨架**：workspace 四 crate 平铺 + proto 契约 + model 保存校验 + Agent 握手闭环 + CI。
- **B2a/B2b/B2c 服务端底座**：配置合并 + SQLite 池/迁移/备份；REST（utoipa + OpenAPI snapshot 守护）+ gRPC 双服务 + rust-embed 内嵌静态资源；pipeline 定义读写闭环；认证与用户体系（setup wizard、session cookie、argon2id、CSRF、登录限流、PAT、项目三档角色、用户管理与注册开关）；机密（主密钥文件 + XChaCha20-Poly1305 加密落库，值只写不读）与审计日志（安全事件只增记账 + 全局 admin 查询）；调度数据底座（builds/jobs/triggers/agents 四表 + 构建号并发单调 + 快照落库 + FIFO 排队 + Agent 标签 AND 匹配 + 触发器基线/探测历史）；Agent 注册面与通道认证（`sisa_` token、心跳在线判定）；engine 编排状态机 + sched 调度下发 + gRPC 任务面接线；构建 REST 面（触发/取消/两种重跑/列表/详情）；触发器（cron + poll）。
- **B3 Agent 全链路**：Agent lib+bin 工程形态 + channel 基座（握手/心跳/退避重连）；注册码换 token；日志 seq 缓冲与断线补传；workspace 隔离；runner host 后端（shell/机密注入/脱敏/日志事件流/进程树终止）；checkout 执行器（git/svn 增量 + ASKPASS 凭据）；runner 容器后端（docker CLI 每步一容器）；cache 模块（files 哈希 + LRU）；upgrader（下载/校验/原子换入/失败退回/排空）；跨平台 tracer bullet + Linux/Windows CI 矩阵。
- **B4 前端**：Vue3 + TS + Vite + vue-i18n 双语工程底座；认证与初始化引导；概览 + 项目页；构建详情页（面包屑 + SSE 日志流 + 产物）；Agent 列表/详情；管理四页（机密/审计/Agent 升级/用户 PAT）；model→TS 生成对账管线（sisyphus-codegen）；混合式 pipeline 编辑器（数据派生轨道 + 表单，无画布——ADR-0020）；tracer bullet headless 冒烟 12 页。
- **B5 收口 + 发布工程**：日志 server 侧全链路（logs 表 + SSE 回放/续传 + 整份下载）；产物链路（存储/端点/Agent 传输/前端）；SCM 真实探测（git ls-remote / svn info + 测试连接 + 分支枚举）；Agent 管理面（升级包上传/指令/排空/版本 + 工作区/缓存清理 + 过旧 Agent 任务面拒连）；保留策略（per-build 30 天清理）；SMTP 通知（全局配置 + 终态发送，TLS 经 ring——ADR-0022）；概览快照 REST + /metrics（七项指标 + 鉴权开关）；admin create headless CLI；全链路 tracer bullet；发布工程（6 目标原生 release 矩阵 + sha256sums + ghcr 双架构 Docker 镜像 + 安装/服务化文档）。

`/api/v1` 全面要求认证，未认证仅 login/setup/register/agent/register 与静态资源可达。设计基线：22 篇 ADR + [CONTEXT.md](CONTEXT.md) 词汇表（见 [docs/adr/](docs/adr/)）。

## License

[MIT](LICENSE)

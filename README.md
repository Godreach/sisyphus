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

实现进行中：B1 骨架（workspace 四 crate、proto 契约、model 保存校验、Agent 握手闭环、CI）与 Spec B2a 存储与 API 底座（配置合并、SQLite 池+迁移+备份、REST/gRPC 双服务、pipeline 定义读写闭环、内嵌静态资源）、Spec B2b 已全量落地——认证与用户体系（setup wizard、登录会话、登录限流、CSRF、PAT、三档角色、用户管理与注册开关）、项目机密服务端面（主密钥文件 + XChaCha20 加密落库 + 机密 CRUD：值只写不读）与审计日志（安全事件只增记账 + 全局 admin 查询端点：按时间/用户/项目/事件类型过滤 + 分页，含 Spec B2b tracer bullet 全链路集成测试）。Spec B2c 已落地调度数据底座——builds/jobs/triggers/agents 四表迁移 + 各自 repo（per-pipeline 构建号并发单调、BuildSnapshot 快照落库读回与 model 等价、FIFO 排队、任务状态全集合迁移与 attempt 重跑、Agent 槽位/标签 AND 匹配、触发器基线/探测历史），以及 Agent 注册面与通道认证——agents REST（全局 admin：建条目签发 `sisa_` token 与一次性注册码、列表/详情含在线/标签/槽位/磁盘占用、启停即踢线、改 max_concurrency/custom_labels）+ gRPC 通道认证（Bearer `sisa_` token，SHA-256 落库、停用即下一连接/下一帧拒）+ 心跳在线判定（15s 收、45s 无心跳判离线，系统标签与磁盘占用随心跳入库）；engine 编排状态机（统一触发入口 + 阶段推进/when 求值 + fail-fast 级联/自动重试 + ResolvedJobSpec 组装）与 sched 调度下发（事件驱动单调度循环、全局 FIFO pending 池、Agent 在线/空槽/标签 AND 匹配、per-Agent 并发槽位从下发到终态、job 超时、build 级取消经通道下发 CancelBuild、Agent 离线转 unknown + orphan 宽限判败、重启从库重建）+ gRPC 任务面接线（JobSpec 下发 / JobAck / JobStatus / JobReported / CancelBuild）已落地，proto 缝 fake Agent 闭环测试（真实 tonic 通道收 JobSpec → ack → 回状态 → 构建推进到终态）通过。Spec B2c-T5 构建 REST 面已落地——手动触发（参数覆盖默认值 + 可选 git 分支/commit 或 svn revision，202 + 构建号）、build 级取消（engine DB 迁移 + 发 `BuildStatus{Cancelled}` 事件，sched 经通道下发 CancelBuild，与 fail-fast 同款事件路径）、两种重跑（from_scratch 新号 attempt=1 / from_failed 同号 attempt+1、已成功任务保留）、构建列表（按号倒序 + 分页 + 状态过滤）与详情（状态/触发人/attempt/耗时/阶段与任务状态），全部 runner 档 `Permission::Run` 授权（触发/取消/重跑 viewer 403、无角色 404 同纪律；列表/详情 viewer 档），缺机密名任务组装期即 failed 且 detail 记名不泄值；sched 周期 drive 非终态构建兜底（事件可丢、DB 是真相源，queued/running 构建不因 BuildCreated 丢失而搁置）；Spec B2c tracer bullet 全链路（Router 缝 REST + proto 缝 fake Agent + 真实调度循环：setup → 建项目存定义 → 建 Agent → 手动触发 → engine 求值组装 → 调度下发 → fake Agent 收任务 ack → 回状态 → 构建推进到终态 → 详情可查 → 从失败任务重跑 attempt+1 → 续跑至成功）通过。Spec B2c-T6 触发器已落地——cron 触发源（`cron` crate 解析 5 字段表达式、按 `(last, now]` 区间命中点取最晚一个触发一次、多 missed 不补跑多份、`last_probe_at` 记命中点去重、默认值 + 默认分支 head 不钉 commit）与 poll 触发源（按项目节奏轮询、创建/启用时记基线不触发、只对之后的新提交触发、commit-id 去重、探测失败记 `last_probe_error` 按节奏重试不自动禁用），SCM 探测经 `scm` 模块 trait 缝隔离（本批假探测 `FakeProbe` 验证基线/节奏/去重/历史逻辑，真实 `git ls-remote`/`svn info` 探测随 scm 批次换入，生产面暂挂 `UnimplementedProbe`——cron 不经探测照常工作）；触发器 CRUD REST（项目 admin 档：列/建 cron 与 poll 各一/改配置与启停；runner 403、无角色 404 同纪律；poll 启用重置基线、删除端点本批不做），config `[triggers] poll_interval_minutes` 默认 5 分钟进触发器 spec，触发器端点 utoipa 注解 + OpenAPI snapshot 随票提交。Spec B3-T1 Agent 工程形态与通道基座已落地——`sisyphus-agent` 升 lib+bin（先例 server #33）：bin 只留启动路径（CLI 解析 → 配置合并 → tracing → 装配组合根 → 常驻），模块实现在 lib 面，`tests/` 集成测试与二进制共用同一组合根；数据目录（`--data-dir`，默认 `~/.sisyphus-agent`）五处约定落位（token / agent.json 文件位 + workspaces/ / cache/ / logbuf/ 子目录），配置面 CLI flag > `SISYPHUS_` 环境变量 > 内置默认（无 config.toml 层），运行日志默认 stderr pretty、`--log-file` 可选追加 JSON（ADR-0019）；channel 基座：token 认证握手（`Authorization: Bearer <sisa_>` metadata + os/arch 系统标签随连接呈送）、版本窗口（Server 过新明确报错拒连）、15s 心跳 + 磁盘占用上报（卷级 statvfs / GetDiskFreeSpaceExW 真实采样 + 缓存/工作区占位）、指数退避重连（1s 起 ×2 上限 60s ±20% 抖动、每次重连 = 新握手 + 认证 + 在途 JobReported + 标签刷新、永久重试不自杀）、单 reader 下行分派骨架（JobSpec/Cancel/Upgrade/Workspace/Cache 指令按类型投递各模块占位 handle）+ 单 writer 保上行写序；测试基座：dev-deps 手写 fake Server（消费 proto 契约、实现 AgentChannel service），loopback 真实 tonic ↔ 真实 agent 组合根，握手/认证/心跳/重连/分派集成测试绿。Spec B3-T2 注册码换 token 已落地——server `POST /api/v1/agent/register`（公开端点：注册码哈希匹配 404 / 一次性未用 409 / 短有效期 403 / Agent 未停用 403 → 签发新 token 换旧 + 注册码置已用，一次性原子闸在 store 层；迁移 0010 加 register_code_used / register_code_expires_at 列，OpenAPI snapshot + 审计 `agent_registered` 随票）与 agent `--reg-key` 引导（reqwest 兑码 → token 落 `<data>/token` 0600 → 常驻直连，失败明确报错退出；dev-deps 极简 HTTP stub 验证请求形态/落盘/错误路径，不依赖 server crate）。Spec B3-T3 日志 seq 缓冲与补传已落地——每 (job, attempt) 一个 jsonl 缓冲文件、事件先落盘 fsync 再活体转发、断线续写、重连幂等重放（Server 按 seq 落库吸收重复）、终态宽限删除 / 孤儿补传后删除（ADR-0007/0013）。Spec B3-T4 workspace 模块已落地——`<根>/<pipeline>/<job>/` 布局 + 名称清洗 / 冲突后缀、`${SISY_WORKSPACE}` 占位替换（host 绝对路径 / 容器内 `/sisyphus/workspace`）、列表 / 清理指令（永不触碰缓存根）、内存级运行中 job 去重、后台低频磁盘占用采样（ADR-0011/0019）。Spec B3-T5 runner host 后端已落地——shell 默认解释器（Unix `sh` / Windows `pwsh` 无则 `cmd`）+ env 注入 + 输出字面量脱敏 `***`（跨块边界）+ 日志事件流编码（stdout/stderr stream 标记、step start/end、per-attempt 单调 seq、超限截断不判败）+ 取消 / 超时进程树终止（Unix `killpg` / Windows `taskkill /T`）+ 终态上报 + 离线终态缓冲补发（ADR-0008/0013/0015）。Spec B3-T6 checkout 执行器已落地——git 增量（clone / fetch + checkout --detach + reset --hard + clean -fd 保 `.git` 与忽略文件）/ svn（cleanup + update -r）、子模块开关、凭据经 ASKPASS 递送永不上命令行、缺二进制清晰报错（ADR-0016）。Spec B3-T7 runner 容器后端已落地——每步一次性 `docker run --rm`、工作区挂载 `/sisyphus/workspace` + HOME 重定向 `.sisyphus-home` + `--user uid:gid`（Linux）、首步前显式 `docker pull`（always）、取消 / 超时 `docker rm -f` 补刀、启动按 label 清扫残留容器、周期 `docker version` 探测喂 `sisyphus/container=docker` 标签（ADR-0018）。Spec B3-T8 cache 模块已落地——key 清洗 / 截断 + files 哈希后缀、restore 在末个 checkout 后 / 其余步骤前、save 仅全步骤成功后、朴素拷贝 + per-key 锁 + temp 原子换入、LRU + 容量上限 + registry JSON 记账、列表 / 删除指令（ADR-0012）。Spec B3-T9 upgrader 已落地——下载（reqwest Bearer）→ sha256 校验 → 原子换入（Unix rename / Windows 改名再写）+ 旧二进制留 `.old` + spawn 新进程 → 连续 3 次启动失败退回 `.old` + 排空（停接新任务、等运行中任务终态）+ 升级阶段经通道上报（ADR-0017）。Spec B3-T10 tracer bullet + 跨平台矩阵已落地——一条全链路集成用例（注册码换 token → 握手认证 → 下发含 shell+checkout+缓存声明的 JobSpec → ack → 真实执行 → 日志 seq 流回 → 断连 → 续跑落缓冲 → 重连幂等补传 → 工作区 / 缓存指令往返 → 升级排空 / 下载 / 校验 / 换入 / spawn → 终态）+ 孤儿上报（Agent 重启 → 孤儿缓冲补传 + 删除 + `JobReported` 空集不认领）+ 两后端切换（host job 宿主机成功、container job 路由 `docker pull` 失败）、取消（进程树终止上报 cancelled）、孤儿上报（Agent 重启 → 孤儿缓冲补传 + 删除 + `JobReported` 空集不认领）各一条绿；tracer 暴露并修复一生产死锁（各 uplink `send` 持 `live` 读锁期间阻塞 `tx.send`，与清理 `set_live(None)` 写锁互锁——断线时 writer 阻塞于上行流控致 agent 卡在清理无法重连；修为克隆 `Sender` 出锁再 `send`、`report_terminal`/`report` 发送失败即落缓冲）；B1 CI 工作流扩展 Linux / Windows 矩阵跑 agent 测试（docker 门控 `#[ignore]` 除外）。B2c→B3 纵切完整闭环成立：注册引导 → 通道认证 → 真实执行（shell/checkout/机密注入/脱敏）→ 日志 seq 流与断线补传 → 缓存 / 工作区管理 → 自升级，Server 侧「下发真实 JobSpec」自此有真实消费方。`/api/v1` 全面要求认证，未认证仅 login/setup/register/agent/register 与静态资源可达。设计基线：21 篇 ADR + [CONTEXT.md](CONTEXT.md) 词汇表（见 [docs/adr/](docs/adr/)）。

## License

[MIT](LICENSE)

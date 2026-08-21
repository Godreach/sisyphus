# 0010 - v1 发布形态与安装体验

日期：2026-08-15
状态：已接受

## 背景

「部署简单」要落到交付物上。已有边界：ADR-0005 rust-embed 内嵌前端（Server 天然单二进制）；ADR-0009 Agent 6 目标交叉编译、依赖树只含 proto；ADR-0007 proto 仅加字段演进、注册码一次性。Agent 自升级机制归 #18，本 ADR 只定策略。盘问过程见 [wayfinder 票 #16](https://github.com/Godreach/sisyphus/issues/16)。

## 决策

### 发布矩阵与产物

- **Server 与 Agent 同为 6 目标发布**（Linux/macOS/Windows × x86_64/aarch64），压缩包 + `sha256sums.txt`，**分级承诺**：受支持平台为 Linux x86_64/aarch64 + Windows x86_64，macOS 两架构 as-is 不承诺（Server 真实部署面窄，交叉编译边际成本低，发布 ≠ 测试覆盖）。
- **server/agent 分开发包**：构建机只下 agent 包不拖 server。命名 `sisyphus-{server,agent}-<ver>-<os>-<arch>.tar.gz|.zip`。
- **官方 Docker 镜像（仅 Server）**：ghcr.io，每版本 tag + `latest`；`linux/amd64`+`linux/arm64` 多架构 manifest；debian-slim 基底，非 root 运行，`/data` 卷，EXPOSE 8080，HEALTHCHECK 打 REST `/healthz`。
- v1 **不做**：安装脚本（curl|sh）、deb/rpm/homebrew/choco 系统包、cosign 签名、agent 官方镜像（与 ADR-0002 agent 默认宿主机直跑相悖）。README 指引 sha256 校验；以上各项等真实需求出现再评估。

### 版本与升级策略

- semver，首个完整版本 **1.0.0**；Server/Agent **同版本号成对发布**。
- 升级顺序强制 **Server 先升**：Agent 版本新于 Server 时启动即拒连并明确报错。
- 兼容窗口 **N-1**：1.x Server 支持上一个 minor 的 Agent（ADR-0007 proto 仅加字段是技术基础）。
  > 修订：版本窗口边角经 [ADR-0017](0017-agent-self-upgrade.md) 补齐——Agent 过旧（< N-1）时任务面拒连（不派任务、UI 显示「版本不兼容」）但升级面保留（版本握手、升级指令、包下载始终开放），窗口滑动不切断自升级通道。
- 升级 = 替换二进制重启；启动时自动跑前向迁移（`sqlx::migrate!` 已编译期内嵌，ADR-0009），**迁移前自动复制 db 文件备份**；不支持降级。

### 数据与配置布局

- 单一 `--data-dir`（默认 `./data`，Docker 固定 `/data`），内含 `sisyphus.db`、`artifacts/`。
- 首次启动生成带注释的 `config.toml` 样例。
- 优先级：**CLI flag > 环境变量（`SISYPHUS_` 前缀）> config.toml > 内置默认**。默认端口 8080。

### 安装引导（Setup Wizard）

- 仅在**用户表为空**时进入 wizard；三步（管理员 -> 首个 Agent -> 首个项目）**各自可跳过**；不带示例 pipeline。
- headless 等价：`sisyphus-server admin create` 等 CLI 命令，跑过即视为引导完成。
- Agent 步展示注册码 + 按目标 OS 生成的一行下载/注册命令供复制。
- **注册码时效：一次性 + 24h 过期**，过期/用掉后管理员可在 UI 重新生成（实现细节归 #10）。

### Agent 首跑体验

- 单命令注册+常驻：`sisyphus-agent --server-url https://… --registration-code XXXX`（或同名环境变量）；注册成功后 token 落本机数据目录并常驻运行。
- 无参数启动时打印缺参提示与示例命令，**不做交互式问答**（headless/脚本化友好）。与 wizard 展示的一行命令完全一致。

### 服务化

- v1 **仅文档示例**：systemd unit 模板 + Windows `sc.exe`/NSSM 指引；不做内置服务安装子命令（服务参数集稳定后再评估）。

## 理由

- 全 6 目标发布 + 分级承诺：既保住「任意平台可下载」的宽门面，又不为 macOS Server 承担持续测试成本。
- 分包 + Docker 双轨覆盖「下载即跑」与「容器化」两类用户，系统包/签名等增量渠道在 v1 用户面未证实时是纯维护负担。
- Server 先升 + N-1：proto 仅加字段让旧 Agent 面对新 Server 天然兼容，N-1 是承诺与测试成本的平衡点；Server 后升的唯一理由不存在。
- 迁移前自动备份 db：单文件 SQLite 让备份成本趋近于零，弥补不支持降级的硬约束。
- wizard 可跳过 + CLI 等价：Docker/headless 用户无法走 web 引导，CLI 等价物是「部署简单」在容器场景的兑现。

## 后果

- 发布流水线需产出 12 个压缩包（6 目标 × server/agent）+ checksums + 双架构 Docker 镜像；macOS Server 产物按 as-is 处理用户反馈。
- `/healthz` 端点进入 Server API 面（#7 日志票、SSE 之外新增的只读端点）。
- `config.toml` 生成逻辑与默认值表进入 server crate 启动路径；`SISYPHUS_` 前缀与 Agent 侧环境变量命名空间需在 #17（工作区）落地时对齐避免混淆。
- Agent upgrader（#18）在本 ADR 约束下工作：升级顺序 Server 先升、兼容窗口 N-1；其拉取渠道即本 ADR 的 Releases 产物。

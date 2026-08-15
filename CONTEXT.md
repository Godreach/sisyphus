# sisyphus 领域词汇表

本项目是对标 Jenkins / 腾讯蓝盾（bk-ci）的 CI 平台。术语以中文词条为准，英文别名为非正式引用。

## 词条

### Server（服务端）

sisyphus 的中心进程。单进程、单二进制部署，承载 web UI、REST API、pipeline 编排调度。不 master/worker 分离，不做高可用集群（v1 设计规模：~100 台 Agent、日千级构建）。

### Agent（代理 / 构建机代理）

跑在构建机上的守护进程，全矩阵支持（Linux/macOS/Windows × x86_64/aarch64）。向 Server 注册，领取任务，在宿主机上直接执行步骤（默认），或按 pipeline 配置选择容器后端。与"多平台"一词的关系：指 Agent 的运行平台，不是构建产物的交叉编译目标。

### Agent 注册（Registration）

Agent 首次接入的引导流程：管理员在 web UI 创建 Agent 条目并生成一次性注册码；Agent 启动时凭注册码向 Server 换取长期 per-Agent 令牌（token，Agent 侧落盘保存）。令牌可单独吊销——UI 禁用 Agent 即踢线。

### 在线状态（Online / Offline）

Agent 的在线与否由通道连接状态判定：连接存活即在线，超时无心跳即离线。离线的 Agent 不接新任务；其运行中任务转入未知态（unknown）--Agent 侧继续执行，重连后回归运行态，超过宽限窗口（默认 10 分钟）未恢复才判失败。

### 标签（Label）

Agent 身上的调度匹配属性，两种来源：管理员自定义标签（web UI 编辑）与系统事实标签（Agent 自动上报的运行平台、架构、容器运行时可用性，不可手编，随实际状态增减）。任务的标签要求为 AND 全集匹配--Agent 必须拥有全部所需标签；选择容器执行环境的任务隐式要求容器标签。

### Pipeline（流水线）

一次交付的编排定义，含参数、阶段与任务。v1 的 Pipeline 定义**只存 Server 端数据库**，通过 web UI 可视化编辑；repo 内没有任何配置文件。这是与 Drone/Woodpecker（repo 内 yaml）的根本区别。同一条 Pipeline 同时只跑一条构建，后来者排队。

### 触发器（Trigger）

使一条 Pipeline 开始执行的事件。v1 支持：手动触发、定时触发（cron）、轮询代码仓库（poll SCM）。poll 在创建/启用时记录当前 head 作基线、不触发构建，只对之后的新提交触发；探测失败记入触发器历史、按节奏重试。webhook 由代码托管平台集成在后续版本引入。

### 步骤（Step）

任务内的最小执行单元，类型仅两种：shell 命令、checkout scm。checkout 调用构建机上的系统 git/svn 客户端（不内嵌 git 库），检出项目绑定的仓库到工作区根并钉到确切提交；git 支持子模块（步骤级开关，默认开）。一次任务一个 SCM 上下文（多仓库检出为 v2 候选）。产物上传/下载不是步骤：上传是任务级声明（完成后上传指定路径），下载是任务级依赖声明（开始前拉取本次构建内其它任务的产物）。步骤在 Agent 的执行环境（宿主机或容器）里跑。

### 任务（Job / 构建）

一次 Pipeline 执行中、绑定到某个执行环境的一个执行单元；日志与产物附着在任务上。任务可配置：执行环境、agent 标签要求、when 条件、环境变量（覆盖 Pipeline 级同名项）、allow_failure、自动重试次数、超时（分钟，默认不限）、产物上传路径与下载依赖。

### 调度（Scheduling）

Server 把就绪任务匹配到 Agent 的过程：中心化全局匹配（无 per-agent 队列），按就绪时间 FIFO，命中「在线 + 有空槽 + 标签齐」的 Agent 即经通道下发，Agent 回执确认。并发槽位是 per-Agent 的任务数上限（Server 端配置，默认 1），从下发占用到任务终态（含产物上传）。无匹配 Agent 时任务无限等待并在界面上标明缺失标签。无优先级、无抢占。

### 槽位（Concurrency Slot）

单个 Agent 可同时执行的任务数额度，Server 端集中计数（含在途下发），占用期为任务下发到终态（含产物上传完成）。

### 取消（Cancel）

用户可取消的最小对象是整个构建：排队中直接移出；运行中经通道下发取消指令，Agent 终止整个进程树并清理未上传产物。失败级联取消（fail-fast）走同一机制。Agent 离线时取消指令挂起，重连后补发。

### 任务规格（Job Spec）

Server 向 Agent 下发的已解析执行规格：变量替换完毕、环境变量合并完毕、when 条件求值完毕、只含待执行节点。Agent 拿到即执行，对 Pipeline 定义本身一无所知。

### 阶段（Stage）

Pipeline 内的编排序：阶段按序执行，阶段内任务并行。不支持任务级 DAG 与跨阶段依赖。

### 条件（when）

阶段/任务/步骤三级均可挂载的受限表达式（比较、与/或、字符串相等、存在性判断），不满足则该节点跳过；阶段跳过即其内任务全不发。可引用内置变量与 Pipeline 参数。

### Pipeline 参数（Parameter）

Pipeline 级的手动输入变量定义：名称、类型（string/number/bool/enum）、默认值、必填、描述。必填参数必须带默认值；任何触发方式的取参语义一律为"默认值，手动触发可覆盖"。

### 内置变量（Built-in Variable）

以 `SISY_` 为保留前缀的 8 个系统变量（构建号、pipeline/项目/任务/阶段名、commit、分支、工作区路径），与用户参数同用 `${name}` 语法引用，可在任意字符串字段中使用。除工作区路径（`SISY_WORKSPACE`，Agent 端在步骤执行前替换，when 表达式禁用）外，其余均在 Server 端下发前解析完毕。

### 环境变量（Env）

Pipeline 级与任务级可声明的键值对，注入 shell 步骤的进程环境；任务级覆盖 Pipeline 级同名项。与 `${}` 变量替换是两回事。

### 构建号（Build Number）

per-pipeline 自增的构建编号（#1、#2…），从 1 起。从头重跑占新号；从失败任务重跑沿用原号，attempt 计数 +1。

### 重跑（Rerun）

手动重新执行：从头重跑生成新构建；从失败任务重跑是原构建的延续（同号 attempt+1，已成功任务的结果/日志/产物保留，失败任务起继续）。

### 修订版本（Revision）

Pipeline 定义每次保存递增的版本号；编辑历史记录操作人与时间。

### 构建快照（Build Snapshot）

每次构建入库的整份 Pipeline 定义 JSON（含所用 revision），保证"某次构建当时到底跑了什么"永远可查。

### 产物（Artifact）

任务执行产生的文件（二进制、报告、压缩包），按任务级声明上传到 Server 端存储，供下载与本次构建内后续任务引用（不跨构建）。

### 执行环境（Execution Environment）

一个任务运行的地方：**宿主机直跑**（默认，零依赖）或**容器**（每任务可选，v1 以 Docker 为主）。

### 项目（Project / 仓库）

Server 端的顶层组织单元，绑定一个 git 或 svn 仓库 URL（含项目级 SCM 凭据，checkout 自动使用），关联 Pipeline 与触发器。git 项目带「默认分支」设置（创建时解析远端 HEAD 预填）；svn 项目 URL 即唯一监控对象、无分支概念。提供可选的连通性「测试连接」验证，不阻塞保存。

### 工作区（Workspace）

Agent 上单个任务的执行目录：`<工作区根>/<pipeline>/<job>/`。同 job 的再次构建与从失败任务重跑复用同一工作区；job 改名即新工作区。复用工作区上 checkout 一律增量（保留 `.git` 与被忽略文件）；清理仅 UI 手动（经通道下发指令），永不触碰缓存目录。各 Agent 的工作区彼此独立、互不同步；v1 仅目录级隔离，并发任务间无 OS 用户/权限边界。

### 缓存（Cache）

任务级声明的跨构建复用目录：每条 = key 模板（支持 `${}` 插值，禁用 `SISY_WORKSPACE`）+ workspace 相对路径列表 + 可选锁文件哈希分量。存储在 `<缓存根>/<pipeline>/<key>/`（独立于工作区，per-pipeline 命名空间，pipeline 改名即重置）。restore 在 checkout 后、save 仅任务成功时，朴素拷贝、保存即不可变快照；容量上限（Agent 本地配置，默认 20 GiB）内 LRU 自动淘汰 + UI 手动删除。各 Agent 缓存彼此独立、互不同步。

### 日志（Log）

任务执行输出的带类型事件流：输出块（stdout/stderr 合流、带 stream 标记）与步骤生命周期事件（start 含命令回显 / end 含退出码）按到达顺序交织，per-job 单调 seq 定位（按 attempt 计）。Agent 经通道流式上报、断线缓冲补传；Server 以压缩 chunk 行入库，保留期与产物共享（默认 30 天），构建记录永久。per-job 体积上限超限截断标记、不判败。web 端单一 SSE 端点按 seq 回放+尾随，另有整份下载。

### 用户 / 角色（User / Role）

简单多用户体系：登录 + 项目级三档角色 viewer/runner/admin（viewer 读、runner 读+触发/取消/重跑、admin 全项目管理含成员分配），不做组织/团队级 RBAC。全局管理员（is_admin）专属全局资源（项目增删、Agent 管理、用户管理、全局配置），并隐含全部项目的项目 admin 权限。账号只禁用不物理删除--历史操作人字段永久保留。注册开关默认关（config），关闭时由全局 admin 建号。

### 会话（Session）

登录后的服务端会话状态，存数据库：HttpOnly cookie（SameSite=Lax）携带、7 天滑动过期、Server 重启不掉线，登出/禁用即失效。配套约定：非安全方法校验同源（CSRF 防护）、登录失败进程内限流（不持久锁定）。密码以 argon2id 哈希存储，永不明文落库。

### 个人访问令牌（PAT）

用户为脚本/CLI 调 REST API 生成的长期凭据：`sis_` 前缀、仅创建时明文可见、库中只存哈希、可选过期、UI 可吊销；以 Bearer 头提交，权限等同 owner 本人。与 Agent 令牌（`sisa_` 前缀）是两个不混用的令牌家族。

### 任务机密（Secret）

pipeline 执行所需的凭据类敏感值，由项目 admin 管理（建/覆写/删），值加密存储（主密钥文件 + AEAD）、永不可回读，仅可列名（viewer/runner 连名不可见）。任务以 `secrets: [NAME, …]` 声明引用，Agent 执行前按名注入进程环境变量；`${}` 插值不解析机密，任务 env 键与机密名冲突在保存时报错，引用不存在的名任务立刻失败。机密值随任务规格下发，输出日志离机前按字面量脱敏为 `***`。

### SCM 凭据（SCM Credential）

项目级特殊凭据（用户名 + 密码/token），checkout 步骤自动使用，不进任务环境变量、无需机密声明引用；Agent 经凭据助手机制（如 GIT_ASKPASS）递给 git/svn 子进程，永不上命令行。与任务机密是两个机制：SCM 凭据绑定项目，任务机密按名引用。

### 审计日志（Audit Log）

安全管理动作的永久记录，仅全局 admin 可见。只录安全事件（登录、用户/Agent/项目/成员/机密/全局配置变更），构建与 pipeline 编辑不入（各自已有操作人记录）；机密操作只记名与操作人、永不记值。v1 不做防篡改（单机无独立可信存储）。

### 通知（Notification）

Pipeline 完成时发出的消息（v1：邮件）。触发规则按 pipeline 级配置：失败必发，成功可配；不按任务粒度发送。

### 初始化引导（Setup Wizard）

首次部署后 web UI 的引导流程：创建管理员、注册第一个 Agent、创建第一个项目。仅当用户表为空时进入；三步各自可跳过，跑过 `admin create` 等 CLI 等价命令即视为引导完成。

### 发布形态（Release Form）

v1 的交付物：GitHub Releases 上 6 目标 × server/agent 分开压缩包 + sha256 校验和 + 仅 Server 的官方 Docker 镜像（双架构、非 root、`/data` 卷）。受支持平台仅 Linux x86_64/aarch64 与 Windows x86_64，macOS as-is。不做安装脚本、系统包、签名。

### 发布节奏（Release Cadence）

Server 与 Agent 同版本号成对发布（semver，首发 1.0.0）；升级顺序 Server 先升（Agent 过新启动即拒连）；兼容窗口 N-1；升级 = 替换二进制重启 + 启动自动前向迁移（迁移前自动备份 db），不支持降级。

### 数据目录（Data Directory）

Server 的单一数据落点（`--data-dir`，默认 `./data`，Docker 固定 `/data`），内含 SQLite 数据库与产物存储；首次启动生成带注释的 config.toml。配置优先级：CLI flag > `SISYPHUS_` 前缀环境变量 > config.toml > 内置默认。

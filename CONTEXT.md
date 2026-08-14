# sisyphus 领域词汇表

本项目是对标 Jenkins / 腾讯蓝盾（bk-ci）的 CI 平台。术语以中文词条为准，英文别名为非正式引用。

## 词条

### Server（服务端）

sisyphus 的中心进程。单进程、单二进制部署，承载 web UI、REST API、pipeline 编排调度。不 master/worker 分离，不做高可用集群（v1 设计规模：~100 台 Agent、日千级构建）。

### Agent（代理 / 构建机代理）

跑在构建机上的守护进程，全矩阵支持（Linux/macOS/Windows × x86_64/aarch64）。向 Server 注册，领取任务，在宿主机上直接执行步骤（默认），或按 pipeline 配置选择容器后端。与"多平台"一词的关系：指 Agent 的运行平台，不是构建产物的交叉编译目标。

### Pipeline（流水线）

一次交付的编排定义，含参数、阶段与任务。v1 的 Pipeline 定义**只存 Server 端数据库**，通过 web UI 可视化编辑；repo 内没有任何配置文件。这是与 Drone/Woodpecker（repo 内 yaml）的根本区别。同一条 Pipeline 同时只跑一条构建，后来者排队。

### 触发器（Trigger）

使一条 Pipeline 开始执行的事件。v1 支持：手动触发、定时触发（cron）、轮询代码仓库（poll SCM）。webhook 由代码托管平台集成在后续版本引入。

### 步骤（Step）

任务内的最小执行单元，类型仅两种：shell 命令、checkout scm。产物上传/下载不是步骤：上传是任务级声明（完成后上传指定路径），下载是任务级依赖声明（开始前拉取本次构建内其它任务的产物）。步骤在 Agent 的执行环境（宿主机或容器）里跑。

### 任务（Job / 构建）

一次 Pipeline 执行中、绑定到某个执行环境的一个执行单元；日志与产物附着在任务上。任务可配置：执行环境、agent 标签要求、when 条件、环境变量（覆盖 Pipeline 级同名项）、allow_failure、自动重试次数、产物上传路径与下载依赖。

### 阶段（Stage）

Pipeline 内的编排序：阶段按序执行，阶段内任务并行。不支持任务级 DAG 与跨阶段依赖。

### 条件（when）

阶段/任务/步骤三级均可挂载的受限表达式（比较、与/或、字符串相等、存在性判断），不满足则该节点跳过；阶段跳过即其内任务全不发。可引用内置变量与 Pipeline 参数。

### Pipeline 参数（Parameter）

Pipeline 级的手动输入变量定义：名称、类型（string/number/bool/enum）、默认值、必填、描述。必填参数必须带默认值；任何触发方式的取参语义一律为"默认值，手动触发可覆盖"。

### 内置变量（Built-in Variable）

以 `SISY_` 为保留前缀的 8 个系统变量（构建号、pipeline/项目/任务/阶段名、commit、分支、工作区路径），与用户参数同用 `${name}` 语法引用，可在任意字符串字段中使用，Server 端下发前完成解析。

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

Server 端的顶层组织单元，绑定一个 git 或 svn 仓库 URL，关联 Pipeline 与触发器。

### 缓存（Cache）

Agent 端按 key 复用的目录（如 ~/.cargo、node_modules），减少重复下载与构建。

### 用户 / 角色（User / Role）

简单多用户体系：注册/登录 + 项目级三档权限 viewer/runner/admin。不做组织/团队级 RBAC。

### 通知（Notification）

Pipeline 完成时发出的消息（v1：邮件）。触发规则按 pipeline 级配置：失败必发，成功可配；不按任务粒度发送。

### 初始化引导（Setup Wizard）

首次部署后 web UI 的引导流程：创建管理员、注册第一个 Agent、创建第一个项目。

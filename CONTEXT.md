# sisyphus 领域词汇表

本项目是对标 Jenkins / 腾讯蓝盾（bk-ci）的 CI 平台。术语以中文词条为准，英文别名为非正式引用。

## 词条

### Server（服务端）

sisyphus 的中心进程。单进程、单二进制部署，承载 web UI、REST API、pipeline 编排调度。不 master/worker 分离，不做高可用集群（v1 设计规模：~100 台 Agent、日千级构建）。

### Agent（代理 / 构建机代理）

跑在构建机上的守护进程，全矩阵支持（Linux/macOS/Windows × x86_64/aarch64）。向 Server 注册，领取任务，在宿主机上直接执行步骤（默认），或按 pipeline 配置选择容器后端。与"多平台"一词的关系：指 Agent 的运行平台，不是构建产物的交叉编译目标。

### Pipeline（流水线）

一次交付的编排定义，含阶段与任务依赖关系。v1 的 Pipeline 定义**只存 Server 端数据库**，通过 web UI 可视化编辑；repo 内没有任何配置文件。这是与 Drone/Woodpecker（repo 内 yaml）的根本区别。

### 触发器（Trigger）

使一条 Pipeline 开始执行的事件。v1 支持：手动触发、定时触发（cron）、轮询代码仓库（poll SCM）。webhook 由代码托管平台集成在后续版本引入。

### 步骤（Step）

任务内的最小执行单元：shell 命令、checkout scm、产物上传、产物下载、通知。步骤在 Agent 的执行环境（宿主机或容器）里跑。

### 任务（Job / 构建）

一次 Pipeline 执行中、绑定到某个执行环境的一个执行单元；日志与产物附着在任务上。

### 阶段（Stage）

Pipeline 内的编排序：阶段按序执行，阶段内任务并行。v1 阶段串行、任务并行。

### 产物（Artifact）

任务执行产生的文件（二进制、报告、压缩包），存到 Server 端存储，供下载与后续任务引用。

### 执行环境（Execution Environment）

一个任务运行的地方：**宿主机直跑**（默认，零依赖）或**容器**（每任务可选，v1 以 Docker 为主）。

### 项目（Project / 仓库）

Server 端的顶层组织单元，绑定一个 git 或 svn 仓库 URL，关联 Pipeline 与触发器。

### 缓存（Cache）

Agent 端按 key 复用的目录（如 ~/.cargo、node_modules），减少重复下载与构建。

### 用户 / 角色（User / Role）

简单多用户体系：注册/登录 + 项目级三档权限 viewer/runner/admin。不做组织/团队级 RBAC。

### 通知（Notification）

任务或 Pipeline 完成时发出的消息（v1：邮件）。

通知是任务级事件驱动，与轮询触发器互不影响。

### 初始化引导（Setup Wizard）

首次部署后 web UI 的引导流程：创建管理员、注册第一个 Agent、创建第一个项目。

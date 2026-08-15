# 0019 - 可观测性：tracing 日志 + 指标双消费 + 事实型警示，无独立告警

日期：2026-08-15
状态：已接受

## 背景

Server 单二进制零依赖（ADR-0004/0005）、调度状态在 SQLite（ADR-0008）、Docker HEALTHCHECK 已定打 `/healthz`（ADR-0010）、#17 裁定 Agent 磁盘占用上报归本票。要定：结构化日志的级别与输出、指标暴露形态（Prometheus 端点 vs 内部计数）、告警 v1 做不做。盘问过程见 [wayfinder 票 #19](https://github.com/Godreach/sisyphus/issues/19)。

## 决策

### 结构化日志（Server/Agent 自身运行日志）

- **crate：`tracing` + `tracing-subscriber`**，生态唯一事实标准。
- **Server 默认 stdout JSON**（Docker/收集器友好），config 可切 pretty 行式（裸机人读）；**不自写日志文件**--systemd journald / Docker 日志驱动天然接管持久化，裸跑用户重定向即可，自管文件与轮转是纯维护负担。
- **Agent 默认 stderr pretty**（构建机常态是无收集器的服务化环境），`--log-file` flag 可选自管追加写 JSON 文件（不自管轮转）。
- **级别**：默认 `info`；`sisyphus` 各模块打语义化事件（调度决策、Agent 上下线、构建起终、触发器命中）；`sqlx=warn` 防逐条 SQL 刷屏（调试时手动 `sqlx=debug`）；axum 访问日志一行制（method/path/status/耗时）。
- **优先级**：`RUST_LOG`（若设置）整体胜出（完整 EnvFilter 语法，power-user 逃生口）> `SISYPHUS_LOG_LEVEL` / `SISYPHUS_LOG_FORMAT` > config `[log]` > 默认 `info`/`json`。

### 指标：facade 双消费

- **`metrics` crate facade 埋点，同一份计数喂两路**：内部快照（REST 出给 UI 概览页）+ `/metrics` Prometheus 文本端点。不绑死 exporter 实现，不预设时序存储。
- **`/metrics` 同端口 8080，默认鉴权**（`Authorization: Bearer sis_…` PAT，任意登录角色；运维可为 Prometheus 专建 viewer 用户），config `[metrics] auth = false` 可关（文档注明仅限可信内网）。不单开端口。
- **首批七项指标**：调度队列深度（含「无匹配 Agent/缺标签」原因分类）、Agent 在线/总数、槽位占用/总量、构建终态计数（成功/失败/取消/超时）、构建时长直方图、产物+日志磁盘占用、gRPC 流断连计数。日志入库延迟与 engine 内部更细指标不埋，等真实需求。
- **UI 只展示当前值**（stat 卡，普通轮询）+ **事实型警示态**：「存在无匹配 Agent 的任务」「有 Agent 离线」「存在排空/不兼容 Agent」--事实判断、零阈值配置；不做内存时序与历史曲线（趋势走 `/metrics` + Grafana 正路）。页面布局归 #15。

### 告警

- **v1 不做独立告警通道**：pipeline 失败通知已有 SMTP（通知票），系统级告警再做就是第二个通知配置面；「调度器卡死」自检是逻辑悖论（卡死的调度器发不出自己的告警），最可靠检测者是外部探针（healthz 超时 + `/metrics` 队列深度）。积压/全离线以概览页警示态覆盖人侧、`/metrics` 覆盖机器侧。v2 候选，届时立票。

### `/healthz` 深度

- 存活 = 进程 + SQLite `SELECT 1`，**不鉴权**（探针惯例，Docker HEALTHCHECK 不带凭据；只答存活不泄业务数据）。
- 调度循环活跃度以「最后调度活动时间」**指标**暴露在 `/metrics`，不进 healthz（healthz 是给编排器的二值信号，复合判断徒增误判面）。

### Agent 磁盘占用上报（#17 移交）

- 心跳常带两路便宜数据：**卷级 free/total**（statvfs / `GetDiskFreeSpaceEx`）+ **缓存占用**（LRU 记账现成值）；**工作区占用**走 Agent 后台任务遍历，默认 10 分钟采样一次（Agent 侧可配），心跳附带最近采样值。
- 全部 proto 追加字段（ADR-0007 仅加字段演进），UI Agent 详情页展示，不做阈值告警。

## 理由

- JSON 默认 + pretty 可切 + 不自写文件：机器与人都在 stdout/stderr 一根管道上取日志，持久化交给进程管理器--这是单二进制部署的自然分工，自管文件生命周期与零依赖承诺相悖。
- 指标双消费成本 ≈ 零（一个 recorder 两路读），既保住 bk-ci 式「UI 自己能看」的监控页，又不锁死接 Prometheus/Grafana 的用户；首批七项每项都有明确消费者（概览卡或 /metrics），不摊大饼。
- 告警不做的三重理由：配置面重复、检测者悖论、警示态+指标已把人和机器两侧的「看得到」覆盖。
- healthz 只探 SQLite：它是唯一可能让整进程挂起的外部状态（WAL 单写者）；调度活跃度给能读指标的人，两信号不混装。
- 磁盘上报按成本分层：贵的（目录遍历）降频后台化、便宜的随心跳走，proto 加字段免费。

## 后果

- **出范围**：OTLP/分布式追踪（tracing facade 在手、按需引入）、UI 指标历史曲线、独立告警通道、日志入库延迟指标。
- proto 心跳消息追加磁盘占用字段组；Server `api` 模块新增 `/metrics` 端点与内部快照端点（进 ADR-0009 模块清单的 api 职责）。
- Agent `channel`（心跳附带）与 `workspace`（后台遍历任务）各加一小块；`metrics` 记录器在 Server 启动路径装配。
- #15（web UI 信息架构）获得新输入：概览页 stat 卡/警示态、Agent 详情页磁盘占用。

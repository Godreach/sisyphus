# 0008 - 任务调度与 Agent 路由模型

日期：2026-08-15
状态：已接受

## 背景

任务（job）怎么从 Server 走到 Agent。上游已定：ADR-0006（阶段串行/阶段内并行、同 pipeline 串行 FIFO、fail-fast 级联、job 的 agent 标签要求字段）、ADR-0007（gRPC 双向流通道、心跳 15s/45s 判离线、取消通道、日志 seq 补传）。本票定匹配、排队、并发、取消、离线处置、超时与崩溃恢复。盘问过程见 [wayfinder 票 #5](https://github.com/Godreach/sisyphus/issues/5)。

## 决策

**能力声明**：双层标签。管理员自定义标签（UI 编辑）+ 系统事实标签（Agent 注册与运行期自动上报，不可手编）：`sisyphus/os`、`sisyphus/arch`；容器运行时由 Agent 定期探测 Docker 可用性上报 `sisyphus/container=docker`，daemon 挂掉标签自动消失。

**匹配语义**：job 的标签要求是纯 tag 列表（`key=value` 亦为字符串形式），AND 全集语义--Agent 必须拥有全部所需标签。job 配置容器执行环境时调度器隐式追加 `sisyphus/container=docker`，用户无感；UI 平台下拉是生成系统标签的语法糖。

**排队与分发**：中心化全局匹配，无 per-agent 队列。可跑 build 当前阶段的并行 job 进入全局 pending 池，调度器按 job 就绪时间全局 FIFO 匹配「在线 + 有空槽 + 标签命中」的 Agent，经 gRPC 流下发，Agent 回 ack 确认。不做公平调度、不做优先级、不做抢占（日千级规模 FIFO 足够；宿主机直跑任务的抢占 = 强杀进程树，破坏大收益小）。

**并发槽位**：Server 端 per-agent `max_concurrency`（UI 可编辑，Agent 注册时上报本机建议值作初始值），默认 1。槽位从下发（ack）占到任务终态--含产物上传完成。调度计数全在 Server 侧中心化（含在途下发），Agent 不做本地排队。

**无匹配 Agent**：无限等待，UI 显示原因（「等待匹配 agent：缺标签 X」），可手动取消。v1 不做排队超时配置。

**取消**：用户入口仅取消整个 build（排队中：移出 pending 池；运行中：经 gRPC 下发 Cancel，Agent 终止整个进程树、清理未上传产物、上报 `cancelled` 终态）；job 级单独取消 v1 不做。fail-fast 级联取消走同一通道。Agent 离线时取消指令挂起，重连补发（Agent 侧发现 build 已取消则本地杀任务）。

**离线处置**：离线不判死。45s 判离线后运行中任务标记 `unknown`，Agent 侧继续执行（日志本地缓冲，重连补传），重连后回归 running。Agent 重启丢任务则上报 aborted 判失败走 fail-fast。orphan 宽限默认 10 分钟（可配），超时未恢复判失败（避免同 pipeline 串行队列被一台掉线机器堵死）；UI 可提前手动判败。

**超时**：仅 job 级 `timeout` 字段（分钟，默认 0 = 无限），Server 从下发时刻计时，超时走取消路径、终态 `timeout`。不做 build 总超时与无输出超时（通道存活即视为在跑）。默认无限：宿主机构建时长差异过大，拍默认值误杀风险大于挂死风险。

**调度器形态**：进程内单调度循环、事件驱动唤醒（槽位释放 / job 就绪 / Agent 上下线 / 标签变更），排队状态落 SQLite（ADR-0004），无内存队列。Server 重启不取消在途任务：通道断开 Agent 侧续跑，重启后 Agent 重连上报在途任务与日志 seq，Server 从库 + 上报重建调度状态。

## 理由

- 全局池 + 中心化匹配避免 per-agent 队列的队头阻塞（A 的专属队列塞满而 B 空闲）；~100 Agent 规模匹配成本可忽略。
- 槽位含产物上传：上传是任务生命周期的固定环节（ADR-0006），未计上传的并发上限会让 Agent 实际负载超卖。
- 离线不判死 + 宽限窗口：构建机短暂掉线（网络抖动、Agent 升级）是常态，立即判败会误杀长任务；无限等待又会堵死串行队列，10 分钟宽限取两者平衡。
- 取消只挂 build 级：与 ADR-0006 的失败语义对称（fail-fast 的作用单位就是 build），job 级取消引入「半途 build」状态机复杂度，v1 不值得。

## 后果

- ADR-0006 任务级字段集追加 `timeout`（分钟，默认 0）。
- Agent 需实现：容器运行时探测上报、进程树终止、离线本地续跑、重启后 orphan 任务上报 aborted。
- Server 调度状态机需容纳 `unknown` 中间态与宽限期计时器。
- proto（ADR-0007）需覆盖：任务下发/ack、Cancel、在途任务上报。
- 解锁 #12（缓存机制--槽位/标签模型已定）、#13（容器后端--docker 探测标签与隐式追加语义已定）、#17（工作区隔离--进程树终止与续跑语义已定）。

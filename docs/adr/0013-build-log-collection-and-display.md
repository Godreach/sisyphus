# 0013 - 构建日志采集与实时展示

日期：2026-08-15
状态：已接受

## 背景

ADR-0007 已定日志走 gRPC 多路复用流、per-job 单调 seq、断线本地缓冲重连补传；ADR-0004 已定 SQLite 批量入库、不落追加文件；ADR-0005 已定 web UI 用 SSE 推日志。本票定剩余语义：DB 存储粒度与压缩、保留策略、体积截断、SSE 端点、流的结构。盘问过程见 [wayfinder 票 #7](https://github.com/Godreach/sisyphus/issues/7)。

## 决策

**派生语义（随既有 ADR 直接落定）**：seq 按 attempt 计（从失败任务重跑 attempt+1 从头计），行按 (build, job, attempt, seq) 定位。Agent 始终回显步骤命令行进日志（step start 事件携带），无关闭项。日志字节原样存储（含 ANSI 色码），前端纯文本渲染时剥离 ANSI。宽限超时判败（ADR-0008）后 Agent 重连：本地缓冲日志仍补传入库挂在原 attempt 上--执行丢弃、日志保留作取证。

**存储粒度与压缩**：DB 一行存一个 chunk。行 = (build, job, attempt, seq 起止, step 序号, stream 标记, 压缩字节, 落库时间)；每块独立 gzip 压缩（flate2/miniz_oxide 纯 Rust 后端），范围读取解压互不依赖。不引入 zstd（C 依赖扩大交叉编译验证面，违背单二进制零依赖原则）。

**保留策略**：日志与产物共享 per-build 保留期，默认 30 天（Server 全局配置），每日低频扫描清理；构建记录（状态、号、时长）永久保留；手动删构建立即全删该构建的日志与产物。

**体积控制**：per-job 日志上限为 Server 全局配置（默认 50 MB），随任务规格下发，Agent 侧超限丢弃并在流内插入截断标记事件，UI 显著标注--截断不判败。不提供 per-job 上限覆盖，不设单行长度上限（总量兜底）。

**SSE 端点**：单一 SSE 端点 per job。`from=<seq>` 起播（缺省 0）：先从 DB 补历史、再接事件总线实时尾随；浏览器原生 EventSource 断线自动重连带 `Last-Event-ID`（即 seq）原地续传；任务终态事件送达并 flush 后关流。另提供整份日志 REST 下载端点（text/plain）。v1 不做独立的分页 REST 日志接口。

**流结构**：带类型的事件流，stdout/stderr 合流。流元素 = 输出块（带 stream 标记）+ 步骤生命周期事件（step start 含命令回显 / step end 含退出码与耗时），单一有序序列按到达顺序交织；UI 按步骤折叠渲染。不存 per-line 时间戳（步骤事件带时间戳，行序即时间序）。

## 理由

- chunk 行 + gzip：日千级构建 × 每 job 数 MB 日志，per-line 入库行数爆炸、SQLite 必然膨胀；纯 Rust gzip 约 4-5 倍压缩比，体积压一个数量级且保住零原生依赖。
- 保留期只删大头：不设保留期的 CI 磁盘和 SQLite 必然无限膨胀；删构建记录丢失构建号连续性与统计，代价与收益不成比例。
- 截断不判败：上限是保护 Server 磁盘的运维手段，不该改变构建成败语义；静默丢弃不可接受，必须显式标记事件。
- 单一 SSE 端点：历史回放与实时尾随用同一 from-seq 语义，EventSource 原生重连即免费获得断线续传；分页 REST 是 SSE 回放 + 总量上限 + 整份下载之外的冗余面。
- 事件流带步骤边界：UI 折叠渲染按步骤组织是刚需；合流保序符合「终端里看到什么就是什么」的直觉。

## 后果

- Server store 模块新增：日志 chunk 表（压缩列）、保留期清理扫描、整份日志下载端点。
- Server events 模块：日志事件接入事件总线（ADR-0009 的 broadcast 热通知），SSE 尾随消费同一流。
- 任务规格（ADR-0006）新增日志上限字段（Server 全局配置解析后随规格下发）。
- Agent runner 模块：步骤命令回显、事件流编码、截断标记、attempt 重置 seq。
- proto（ADR-0007）：日志消息携带 stream 标记与步骤生命周期事件类型。
- 前端日志视图（#15 归属）：SSE 消费、ANSI 剥离、步骤折叠、截断标注。

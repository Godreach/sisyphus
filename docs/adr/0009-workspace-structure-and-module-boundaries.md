# 0009 - cargo workspace 结构与 Server/Agent 模块划分

日期：2026-08-15
状态：已接受

## 背景

技术栈已全部落定（axum+tonic 同进程、SQLite+sqlx、Vue 3 前端内嵌），开工前定 monorepo 的 crate 切分与模块边界。硬约束：Agent 全矩阵交叉编译（6 目标）不背 Server 重依赖；proto 是 Agent/Server 唯一共享契约（ADR-0007）；为 `web/` 留位（ADR-0003）；trait 缝已定在存储层（ADR-0004）。盘问过程见 [wayfinder 票 #9](https://github.com/Godreach/sisyphus/issues/9)。

## 决策

### Workspace 拓扑

- **单一 cargo workspace**：根 `Cargo.toml` + 单 lockfile + `workspace.dependencies`/`workspace.lints` 统一版本，v1 不做 Server/Agent 分家。
- **v1 共 4 个 crate**，目录**平铺在仓库根**（不做 apps/libs 二分，不设 `crates/` 中间层），与 `sisyphus-web`、`docs/` 平级（2026-08-15 经 ADR-0021 修订）：
  - `sisyphus-proto` -- `.proto` 生成物（tonic/prost）。**Agent/Server 唯一共享 crate**。`.proto` 源文件放 `sisyphus-proto/proto/`（契约先行、语言中立），build.rs 指向之。
  - `sisyphus-model` -- Pipeline 定义 JSON 模型、when 表达式 AST、参数/内置变量解析、编辑器保存校验规则的纯类型与纯逻辑（serde），零重依赖叶子 crate；未来 TS 类型生成以它为锚点。
  - `sisyphus-server` -- 单进程承载全部 Server 职责。2026-08-16 起 **lib+bin**（票 #33）：bin 只留启动路径，模块实现在 lib 面——`tests/` 集成测试与二进制共用同一 Router/组合根装配（Spec B2a 测试缝：进程内 oneshot，不起 socket）。模块边界不变：pub 面即 crate 本身，模块间仍走 crate 内可见性。
  - `sisyphus-agent` -- bin，只依赖 `sisyphus-proto`，不依赖 `sisyphus-model`。
- **proto 生成物不进 git**：`sisyphus-proto` build.rs 用 tonic-build/prost-build（vendored protoc，无系统依赖）现场生成。
- **不立 `xtask`**：OpenAPI snapshot、i18n 对账、TS 生成等工具需求真实出现时再建。

### Server 模块（crate 内模块，不上 pub 边界）

`api/`（REST+SSE+静态）、`grpc/`（agent 通道）、`engine/`（构建编排状态机：阶段推进、when 求值、fail-fast、重跑 attempt）、`sched/`（调度与 agent 路由）、`trigger/`（cron/poll 扫表）、`scm/`（server 侧浅比较，#11 细化）、`auth/`（#10 细化）、`store/`（SQLite+迁移+`LogStore`/`ArtifactStore` trait 实现）、`events/`（进程内 tokio broadcast 总线）、`notify/`（SMTP）。

- **模块不预升 crate**：自有代码量摊不满，pub 仪式与环依赖（engine↔sched 共享状态）是实代价。哪一模块长出第二个消费者（如 scm 被 agent 复用）再升。
- **事件总线只做热通知**：SSE 收到通知后按 Last-Event-ID/offset 从 DB 读增量（ADR-0005 重放兜底），broadcast 丢消息无害；事件类型是进程管线 enum，不进 model。
- **迁移 SQL** 放 `sisyphus-server/src/store/migrations/`，`sqlx::migrate!` 编译期嵌入（单二进制自带迁移）；**`.sqlx` 离线校验文件进 git**（CI 免 DATABASE_URL 直接构建）。

### Agent 模块（crate 内模块）

`channel/`（gRPC 连接管理、重连、日志 seq 缓冲落盘）、`runner/`（host/容器两执行后端）、`workspace/`（#17 细化）、`cache/`（#12 细化）、`upgrader/`（#18 细化）、`cli`（clap）。

### 下发边界：ResolvedJobSpec 形态

- proto 里定义**已解析任务规格**：变量已替换、env 已合并、三级 when 已求值、只含该跑的节点。server `grpc` 模块下发前组装，Agent 拿到即执行，对 Pipeline 模型一无所知。
- **when 三级全部 server 求值**（求值器在 `sisyphus-model`，server 独享）；步骤级 when 引用 `SISY_WORKSPACE` 在编辑器保存时校验报错。
- **SCM 不预建共享 crate**：server `scm` 模块（poll 浅比较）与 agent `runner`（checkout 深操作）各自实现；#11 落地时发现真实重复再抽。

## 理由

- **Agent 依赖树最小化是交叉编译矩阵的护城河**：agent 只依赖 proto，model/engine/store 的任何演化不触发 agent 重编，6 目标发布矩阵按 `-p sisyphus-agent` 裁剪依赖。
- **model 单独成 leaf 是编译并行度收益最大点**（改模型不重编 axum/sqlx），且同时锚定编辑器校验、快照存储、TS 生成三个消费者。
- **单 workspace 的编译并行度 ≈ crate 数量级的函数**：并行单位是 crate 内的 codegen unit 与 crate 间的 pipeline，server 内部切模块不损并行度；真正的并行收益已由 proto/model/agent/server 四分拿到。
- 分家 workspace 解决不了真问题（`-p` 已裁剪依赖树），还把 proto 共享变成 path/git 依赖的别扭事。
- 生成物进 git 的唯一收益（review diff）由 CI artifact/`cargo expand` 替代。

## 后果

- 模块升 crate 的触发器明确：**第二个消费者出现**。届时升 crate 裁量权归对应细化票（#11 scm 等）。
- `sisyphus-model` 承载编辑器保存校验（参数默认值、when 受限语法、`${SISY_WORKSPACE}` 禁入 when 等），前端 TS 校验逻辑以其为唯一事实源，漂移靠生成/对账工具兜底（首个工具需求出现时立 xtask）。
- SSE/事件语义定了"通知可丢、DB 重放兜底"，api 模块读路径与 store 写路径解耦。
- 解锁 #5（调度票：sched 模块细化）、#7（日志票：events+store 读路径已留位）、#10/#11（auth/scm 模块内部设计）；发布工程票（#16）拿到 crate 清单与 agent 依赖树边界。

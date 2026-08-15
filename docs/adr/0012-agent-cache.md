# 0012 - Agent 端缓存机制

日期：2026-08-15
状态：已接受

## 背景

ADR-0002 定了默认宿主机直跑，ADR-0011 定了工作区模型与缓存边界：缓存根独立于工作区根（`<agent 数据根>/cache/`）、清理工作区永不触碰缓存、各 Agent 互不同步，key 设计与淘汰规则留给本票。缓存是跨构建复用的核心价值，也是 CI 磁盘打满的头号来源，本票定全部语义。盘问过程见 [wayfinder 票 #12](https://github.com/Godreach/sisyphus/issues/12)。

## 决策

**声明形态**：任务级声明（与产物声明同级），列表形态，无条数上限。每条缓存 = `key`（字符串模板）+ `paths`（workspace 相对路径列表）+ 可选 `files`（参与哈希的锁文件列表，仅精确路径、不支持 glob）。

**key 规则**：`key` 为用户字符串模板，支持 `${}` 插值（含 `SISY_BRANCH` 等内置变量）；`files` 声明时按声明顺序拼接文件内容取 sha256 前 12 位 hex，以 `-<hash>` 后缀拼在用户 key 后。锁文件变更自动 miss，是缓存正确性的主力保障。不做前缀回退（restore-keys）：files 哈希已覆盖其主要价值，回退引入多匹配与淘汰歧义。

**key 约束**：`SISY_WORKSPACE` 禁止出现在 key（per-Agent 值会让每台机器 key 永不相同），Server 保存 pipeline 时校验拒绝，与 when 表达式禁用同款处理。插值后的 key 由 Agent 侧清洗目录名（非 `[A-Za-z0-9._-]` 替换 `_`）、超长截断（目录名 255 上限，为 hash 后缀留位）。Server 保存时仅校验模板字面部分（非空、长度上限、字符集），不阻止插值。

**路径语义**：`paths` 仅允许 workspace 相对路径，不支持家目录绝对路径。家目录缓存（`~/.cargo` 等）用环境变量重定向进 workspace（如 `CARGO_HOME=${SISY_WORKSPACE}/.cargo`）再缓存该目录。容器任务挂载同一 workspace，天然复用同一套缓存机制。

**存储布局与隔离**：per-pipeline 命名空间 `<缓存根>/<pipeline>/<清洗后 key>/`，与工作区布局 `<工作区根>/<pipeline>/<job>/` 同构。pipeline 改名 = 缓存重置（与工作区改名语义对齐），旧命名空间成孤儿由 LRU 兜底回收。key 撞名不跨 pipeline 互相污染；同机多 pipeline 各存一份依赖缓存是接受的代价，跨 pipeline 共享留 v2。

**restore/save 时机**：restore 在最后一个 checkout 步骤完成后、其余步骤之前（锁文件就位才能算 files 哈希；无 files 分量则等同首步骤前）。save 在全部步骤成功后、产物上传之前执行（判据是步骤成功，与上传成败解耦）；取消/超时/失败任务一律不 save；被 when 跳过的任务无缓存动作。

**失败语义**：restore miss = 冷启动照常跑。`files` 声明的锁文件缺失 = fail-fast，任务立即失败并报错写明缺哪个文件（typo 症状「永不命中且无报错」不可排查）。restore 拷贝中途失败 = 告警并当 miss 继续（缓存是优化不是依赖，磁盘抖动不打挂构建）。save 失败 = 告警不判败。save 时 paths 全部缺失 = 跳过保存并告警；部分存在 = 存在即存。

**传输机制**：v1 朴素拷贝。缓存一旦保存即为不可变快照，构建期间对 workspace 的任何写入不会穿透污染缓存。硬链接优化（同盘近零成本）记为 v2--对「工具原地改文件」的行为敏感，不进 v1。

**并发语义**：per-key 文件锁 + temp 目录原子换入。restore 持共享读锁；save 持独占锁、先写 `<key>.tmp-<uuid>/` 再换名顶替。并发结果 last-writer-wins（两个并行 job 各 restore 旧缓存、各自跑、先后 save 相互覆盖），语义可预期且无害。不做内容寻址去重。

**淘汰**：三层叠加。per-Agent 容量上限默认 20 GiB（Agent 本地配置，0 = 不限）+ LRU 自动淘汰 + UI 手动删除。LRU 时钟 = 最近一次 restore 时间；save 后淘汰最久未用者直到回到上限内；单条超过上限直接跳过保存并告警；淘汰跳过正被读/写锁持有的 key。Agent 侧本地 registry（单 JSON 文件原子写，key -> 大小/最近使用）记账，Server 只做展示转发。

**UI**：per-Agent 缓存列表（key、大小、最近使用、删除按钮），按 pipeline 分组展示，支持单 key 删除与该 Agent 全清，经通道查询/指令下发，与工作区列表同级同款。不做 per-pipeline 批量清空。

## 理由

- 任务级而非 pipeline 级声明：读写缓存的主体是 job，pipeline 级无法表达「依赖安装 job 写缓存、其它 job 只读」的常见分工。
- 仅相对路径：绝对路径把缓存与特定机器布局耦合；相对路径让容器任务零额外设计复用缓存。
- fail-fast 的 files 缺失：静默宽容的 typo 症状是「缓存永不命中」，用户无从排查；把错误推到最早时点。
- 朴素拷贝而非硬链接：语义干净压倒性能，硬链接的原地写穿透风险不该由 v1 背。
- 自动 LRU 而非像工作区那样仅手动：工作区可能有不可再生的本地状态，缓存是纯再生数据，自动淘汰在哲学上是安全的；不做自动淘汰的 CI 缓存最终都以磁盘打满收场（GitLab 前车之鉴）。
- 容量上限放 Agent 本地配置：磁盘容量是机器的运维属性，不参与任何调度决策（对比：并发槽位放 Server 是因为它参与调度）。

## 后果

- Agent cache 模块（ADR-0009）需实现：key 清洗、files 哈希、restore/save 拷贝、per-key 锁与原子换入、LRU 淘汰与 registry、通道的列表查询/删除指令处理。
- 通道（ADR-0007 的 proto）需覆盖：缓存列表查询与删除指令两类消息，与工作区列表/清理同款。
- 任务规格（ADR-0006）新增缓存声明字段；Server 保存校验新增：key 模板字面部分、key 禁用 `SISY_WORKSPACE`、paths 仅相对路径、files 仅精确路径。
- #13（容器后端）获得约束：容器任务挂载同一 job 工作区，缓存 restore/save 由 Agent 宿主侧在挂载目录上执行，容器内无感知。
- 跨 pipeline 缓存共享、硬链接优化留 v2。

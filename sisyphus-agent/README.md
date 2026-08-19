# sisyphus-agent

sisyphus Agent：构建机守护进程（lib + bin，**只依赖 `sisyphus-proto`**，ADR-0002/0009）。全矩阵支持 Linux / macOS / Windows × x86_64 / aarch64。

## 定位

跑在构建机上的守护进程：向 Server 注册、领任务、在宿主机直接执行步骤（默认，零依赖）或按任务配置选容器后端（v1 以 Docker 为主）。lib + bin 结构（ADR-0009）——`src/main.rs` 是薄壳，全部模块在库面 `sisyphus_agent`，集成测试与二进制共用同一组合根。**只依赖 `sisyphus-proto`**，不依赖 model / server——对 Pipeline 定义一无所知，只消费已解析的 `JobSpec`。

## 启动路径（`src/main.rs`）

解析 CLI → 合并配置（ADR-0010）→ 初始化 tracing（stderr pretty + 可选 `--log-file` JSON，ADR-0019）→ 启动清扫残留容器（按 `sisyphus.managed=true` label，best-effort）→ 注册引导（`--reg-key` 存在则先兑 token 落盘）→ `Agent::new(config)` 装配组合根 → 常驻运行（断线指数退避重连，永久重试不自杀）。

## 模块结构

| 模块 | 说明 |
| --- | --- |
| `channel` | gRPC 连接管理：token 认证握手（`Bearer sisa_`）、版本窗口（Server 过新拒连）、15s 心跳 + 磁盘占用上报、指数退避重连（1s 起 ×2 上限 60s ±20% 抖动、永久重试）、单 reader 分派 + 单 writer 保写序（ADR-0007/0008/0017/0019） |
| `runner` | `JobSpec` → ack → 步骤序贯执行 → 终态上报：host + 容器两后端（只差"进程怎么起"）、内存级同 job 去重、取消/超时进程树终止、离线终态缓冲补发（ADR-0002/0006/0008/0013/0015/0018） |
| `exec` | 宿主机进程执行：默认解释器（Unix `sh` / Windows `pwsh 无则 cmd`）、cwd/env 注入、进程树终止（Unix `killpg` / Windows `taskkill /T`）、超时/取消竞争回收 |
| `checkout` | checkout 执行器：shell 出系统 git/svn（不内嵌库）；git 增量（clone/fetch + checkout --detach + reset --hard + clean -fd）/ svn 增量、子模块开关、凭据经 ASKPASS 递送永不上命令行（ADR-0016） |
| `container` | 容器后端：每步一次性 `docker run --rm`、工作区挂载 `/sisyphus/workspace` + HOME 重定向 + `--user uid:gid`（Linux）、env 文件 + ASKPASS 挂载、取消/超时 `docker rm -f` 补刀、启动清扫残留、周期探测喂 `sisyphus/container=docker` 标签（ADR-0018） |
| `stepio` | 步骤 IO 共享：输出流式编码 + 步骤生命周期事件 + per-job 日志截断（shell 与 checkout 共用，ADR-0013） |
| `redact` | 任务机密输出字面量脱敏：跨输出块边界有状态流式、最长匹配优先、无机密直通零开销（ADR-0015） |
| `logbuf` | 日志 seq 缓冲：每 (job, attempt) 一个 jsonl 文件、事件先落盘 fsync 再活体转发、断线续写、重连幂等重放（Server 按 seq 落库吸收重复）、终态宽限删除/孤儿补传后删除（ADR-0007/0013） |
| `workspace` | 工作区管理：`<根>/<pipeline>/<job>/` 布局 + 名称清洗/冲突后缀、列表/清理指令（永不触碰缓存根）、运行中 job 去重、后台低频占用采样、`${SISY_WORKSPACE}` 占位替换（ADR-0011） |
| `cache` | 跨构建缓存：key 清洗 + files 哈希后缀、restore 在末个 checkout 后/其余步骤前、save 仅全步骤成功后、朴素拷贝 + per-key 锁 + 原子换入、LRU + 容量上限 + registry 记账、列表/删除指令（ADR-0012） |
| `upgrader` | 自升级：排空 → 下载（Bearer）→ sha256 校验 → 原子换入 → spawn 新进程；任一步失败保持旧版、连续 3 次启动失败退回 `.old`；阶段经通道上报（ADR-0017） |
| `register` | 注册引导：凭一次性注册码向 Server REST 端点换长期 token 落 `<data>/token`（0600）；失败明确报错退出（票 #57） |
| `config` | 启动配置：CLI flag > `SISYPHUS_` env > 内置默认（无 config.toml 层）；数据目录五处约定 |
| `windows_job` | Windows 作业对象进程树终止（`cfg(windows)`）——`TerminateJobObject` 一次杀干净含孤儿（ADR-0008） |

## 组合根（`src/lib.rs` `Agent`）

`Agent` 装配全部模块：配置 + 通道参数 + 下行分派 + 在途任务集 + 日志缓冲 + runner/workspace/cache/upgrader 各模块句柄 + 排空闸门 + 升级 exit watch。

- `Agent::new(config)`：默认通道参数（15s 心跳、1s/×2/60s/±20% 退避）+ real 升级依赖。
- `Agent::with_channel_config(...)`：测试注入短心跳/短退避/固定采样求确定性。
- `Agent::with_upgrader_deps(...)`：测试注入 fake 下载器/启动器（不真下载/真重启进程）。
- `Agent::run(shutdown)`：常驻——占位模块循环 + 通道重连循环；断线退避、升级成功即旧进程退出。

## 配置

- **优先级**：CLI flag > `SISYPHUS_` 前缀环境变量 > 内置默认（ADR-0010；**Agent 无 config.toml 层**）。
- **数据目录**（`--data-dir`，默认 `~/.sisyphus-agent`）五处约定：`token`（per-Agent 凭据，0600）、`agent.json`（本地状态）、`workspaces/`、`cache/`、`logbuf/`。
- **日志**：`RUST_LOG` 整体胜出 > `SISYPHUS_LOG_LEVEL` > 默认 `info`；stderr pretty 常开，`--log-file` 可选追加 JSON。

| 配置项 | CLI flag | 环境变量 | 默认 |
| --- | --- | --- | --- |
| Server gRPC 地址 | `--server-url` | `SISYPHUS_SERVER_URL` | 无（缺则启动失败） |
| Server REST 基址 | `--api-url` | `SISYPHUS_API_URL` | 无（仅 `--reg-key` 注册时需要） |
| 一次性注册码 | `--reg-key` | — | 无 |
| 数据目录 | `--data-dir` | `SISYPHUS_DATA_DIR` | `~/.sisyphus-agent` |
| 工作区根 | `--workspace-root` | `SISYPHUS_AGENT_WORKSPACE_ROOT` | `<data>/workspaces` |
| 日志级别 | `--log-level` | `SISYPHUS_LOG_LEVEL` | `info` |
| 日志文件 | `--log-file` | `SISYPHUS_LOG_FILE` | 无（走 stderr） |
| 缓存容量上限（GiB，0=不限） | `--cache-capacity-gib` | `SISYPHUS_CACHE_CAPACITY_GIB` | 20 |

**首次接入**：管理员在 web UI 建 Agent 条目并签发一次性注册码（24h 有效）后：

```bash
sisyphus-agent --server-url http://<server>:50051 --api-url http://<server>:8080 --reg-key sisa_reg_xxx
```

注册成功 token 落 `<data>/token`，后续启动读 token 直连、不再需要注册码。

## 构建

```bash
cargo build -p sisyphus-agent
cargo test -p sisyphus-agent
cargo clippy -p sisyphus-agent -- -D warnings
```

### 测试纪律

`tests/` 经 `Agent::with_channel_config` 注入短心跳/短退避与手写 fake Server（消费 proto 契约、实现 `AgentChannel` service）对跑，loopback 真实 tonic ↔ 真实 agent 组合根，不起独立进程。真实 docker 执行（pull/run/清扫/探测）单测门控 `#[ignore]`（需 daemon）；CI 在 Linux/Windows 矩阵跑 agent 测试，docker 门控项除外。

## 与其它 crate 的关系

- **只依赖 `sisyphus-proto`**——不依赖 model / server，是 workspace 里和 proto 并列的叶子执行端。
- 经 gRPC 通道（proto）与 `sisyphus-server` 交互：收 `JobSpec`/`CancelBuild`/升级/工作区/缓存指令，回 `JobAck`/`JobStatus`/日志流/`JobReported`/升级状态。

## 参见

- [ADR-0002](../docs/adr/0002-agent-host-execution-with-optional-containers.md)、[ADR-0007](../docs/adr/0007-agent-server-grpc-channel.md)、[ADR-0008](../docs/adr/0008-scheduling-and-agent-routing.md)、[ADR-0011](../docs/adr/0011-agent-workspace-isolation-and-lifecycle.md)、[ADR-0012](../docs/adr/0012-agent-cache.md)、[ADR-0013](../docs/adr/0013-build-log-collection-and-display.md)、[ADR-0017](../docs/adr/0017-agent-self-upgrade.md)、[ADR-0018](../docs/adr/0018-container-backend-integration.md)
- [顶层 README](../README.md)（Agent 配置 / 首次接入）、[CONTEXT.md](../CONTEXT.md)（Agent / 工作区 / 缓存 / 日志 / 自升级 等词条）

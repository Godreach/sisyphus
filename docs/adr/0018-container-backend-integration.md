# 0018 - 容器后端集成：shell 出 docker CLI + 每步骤一次性容器 + image 单字段

日期：2026-08-15
状态：已接受

## 背景

ADR-0002 定了默认宿主机直跑、容器为每任务可选后端。上游约束已齐：调度隐式追加容器标签与 Agent 周期探测 Docker 可用性（ADR-0008）、容器任务挂载同一 job 工作区且缓存 restore/save 由宿主侧在挂载目录上执行（ADR-0011/0012）、checkout shell 出系统 git/svn 客户端且凭据走 ASKPASS 机制（ADR-0016）、机密 env 注入与 Agent 侧脱敏（ADR-0015）、步骤在执行环境里跑（CONTEXT.md 步骤词条）。本票定容器后端的全部集成语义：调用方式、执行模型、挂载与入口、镜像与凭据、配置面、清理与平台支持。盘问过程见 [wayfinder 票 #13](https://github.com/Godreach/sisyphus/issues/13)。

## 决策

**实现形态：Agent shell 出系统 `docker` CLI**（与 ADR-0016 shell 出 git/svn 同模型）

- 不引入容器 Rust 库（bollard/Engine API、containerd）：装了 daemon 的机器必有 CLI；agent crate 依赖树保持干净（ADR-0009）。
- 日志直接继承 CLI stdio 流式进既有日志链路（ADR-0013），退出码天然传递；Windows named pipe、Docker Desktop 路径翻译全由 CLI 兜底。
- 探测：周期执行 `docker version` 成功即上报 `sisyphus/container=docker`，失败标签自动消失（ADR-0008 语义不变）。
- Engine API（bollard）留 v2 候选（出现 exec 会话管理类需求再议）。

**执行模型：每步骤一个一次性容器**

- 每个步骤一次 `docker run --rm`，同任务所有步骤挂载同一工作区：文件系统状态跨步骤保留、env 每步重注入、无共享进程状态--与宿主机后端的步骤语义完全同构，runner 只有一套步骤编排。
- 无常驻容器、无 `docker exec` 会话管理。

**挂载点、路径与入口（全部固定，不可配）**

- 工作区挂载点固定 `/sisyphus/workspace`，工作目录 `-w /sisyphus/workspace`；仅此一个挂载（缓存在工作区内，ADR-0012）。
- `${SISY_WORKSPACE}` 占位符在容器任务里替换为容器内路径（ADR-0011 的 Agent 侧替换机制天然支持）。
- shell 入口固定 `/bin/sh -c`；镜像缺 `sh` = 清晰报错（同「缺 git 二进制」哲学）；bash/pwsh 偏好由用户烙进镜像。

**容器用户与 HOME**

- Linux 宿主固定 `--user <agent uid:gid>`：把 Agent 进程自身的 uid/gid 映射进容器，避免容器内 root 在挂载工作区落盘、卡死宿主侧缓存 save 与 UI 手动清理（Agent 以低权限服务账号运行，ADR-0011）。
- HOME 重定向到工作区内隐藏目录（`/sisyphus/workspace/.sisyphus-home`）：可写、跨步骤持久、随工作区清理回收。
- 代价明示：容器内步骤无 root（包安装类步骤靠镜像预装）、镜像默认用户被覆盖。job 级 user 覆盖留 v2。

**checkout 在容器内执行，不特例**

- 镜像前置要求：带 checkout 步骤的容器任务，镜像必须含 git ≥ 2.20 / svn ≥ 1.10（同 ADR-0016 版本与文档化要求），缺二进制 = checkout 步骤清晰报错。
- SCM 凭据沿用 ADR-0016 机制族：Agent 生成 ASKPASS 小脚本挂载进容器（`/sisyphus/askpass.sh`），凭据值经临时 env 文件进容器环境、任务毕即删；永不上命令行。`docker inspect` 可见容器 env，与宿主机 env 注入同威胁模型，不新增暴露面。

**镜像拉取与私仓凭据**

- 任务开始时（首步骤前）显式 `docker pull` 一次，固定 always 语义；pull 失败 = 任务失败，错误信息清晰（401 类提示到 Agent 宿主机 `docker login`）。
- 私有 registry：v1 完全使用宿主机 daemon 既有登录态（`config.json`），sisyphus 不托管、不下发 registry 凭据--不新增机密面，与「缓存/工作区是 per-Agent 本地资源」哲学一致。
- pull policy 配置、Server 托管 registry 凭据留 v2。

**配置面：v1 仅 `image` 一个字段**

- 网络默认 bridge、无额外挂载、无 privileged、无 user/pull policy/shm-size 透出；需要特殊环境的用户烙镜像。任意挂载/privileged 是容器逃逸面，低配字段集让安全边界一句话说清。

**取消、超时与孤儿容器清理**

- 容器命名 `sisyphus-<jobid>-<attempt>-<stepseq>-<短随机>`，加归属 label。
- 三层清理：正常路径 `--rm` 自清；取消/超时在杀掉 CLI 进程树后按名 `docker rm -f` 补刀（幂等）；Agent 启动时按 label 清扫一次残留容器（兜住 CLI 被 SIGKILL 的窗口）。孤儿容器占着 daemon 资源且可能还在写挂载目录，必须主动回收。

**平台支持**

- v1 容器后端只承诺 Linux 宿主（Linux 容器），测试矩阵仅 Linux。
- macOS/Windows 宿主上跑 Docker Desktop 类 Linux 引擎：探测到即上报标签、照常执行，文档标注 as-is（挂载性能、路径翻译全信 docker CLI，不自建翻译层）。
- Windows 原生容器明确不做。

## 理由

- **CLI 而非 Engine API 库**：与 ADR-0016 同一决策逻辑--要用容器就必然装有 CLI；bollard 是一条重依赖树且 6 目标交叉编译面（ADR-0010）多一份风险；日志/退出码/平台怪癖由 CLI 兜底是白赚的健壮性。
- **每步一容器**：与宿主机后端步骤语义同构是最大收益--两后端只差「进程怎么起」，编排、日志、取消、脱敏全部复用；exec 模型的容器生命周期与会话管理是纯复杂度零收益。
- **checkout 不特例**：模型纯粹压倒前置便利；镜像要求文档化即可（官方镜像已捆 git+svn 的先例，ADR-0016）。
- **`--user` 映射**：Jenkins `.inside()` 默认加 `-u` 的教训--root 属主文件让宿主侧一切后续操作（缓存换入、工作区清理）失效；无 root 的代价（不能容器内 apt）由镜像预装承接。
- **always pull**：latest 标签的正确性压倒速度；镜像未变时只做 manifest 校验，层不走网络，代价可接受。
- **宿主机登录态**：registry 凭据是机器运维属性（同缓存容量上限的归类逻辑），托管进 Server 会扩大机密面且引入下发链路，v1 无收益。

## 后果

- Agent runner（ADR-0009）新增容器执行后端：docker run 编排、命名/label、pull、三层清理；「执行环境」抽象（ADR-0002）自此有两个实现（host/container）。
- 任务规格与 pipeline 模型：容器执行环境仅 `image` 字段；调度隐式容器标签不变（ADR-0008）。
- env 递送统一走临时 env 文件（机密 + SCM 凭据 + 任务 env），任务毕即删，不上命令行；日志脱敏链路（ADR-0015）不变。
- 文档需写明：容器镜像前置（sh、按需 git/svn）、`docker login` 私仓说明、非 Linux 宿主 as-is 边界。
- 解锁下游：#15（web UI--image 字段入 pipeline 编辑器）。
- v2 候选：Engine API、job 级 user 覆盖、额外挂载/network/pull policy/shm-size 透出、Server 托管 registry 凭据。

# 0016 - SCM 集成层：shell 出系统 git/svn 客户端 + 服务端中心化 poll + 全量克隆增量更新

日期：2026-08-15
状态：已接受

## 背景

git 与 svn 两种 SCM 的客户端集成：Agent 端 checkout、Server 端 poll 探测、克隆/增量策略、分支语义。已有约束：SCM 凭据形态已定（ADR-0015：项目级用户名+密码/token、ASKPASS 类机制递送、永不上命令行）、工作区增量语义已定（ADR-0011：per-job 持久工作区、保 `.git` 与忽略文件）、poll 节奏已定（ADR 未编号、票 #14：项目级默认 5min、同仓库串行、commit-id 去重）、任务规格经通道下发（ADR-0007）。盘问过程见 [wayfinder 票 #11](https://github.com/Godreach/sisyphus/issues/11)。

## 决策

**实现形态：Agent 端与 Server 端都 shell 出系统 git/svn 客户端**

- Agent 端 checkout：shell 出系统 `git` / `svn` 二进制（Jenkins 模型）。不用 libgit2（git2 的 https/ssh 传输拖 openssl/libssh2 C 依赖）、不用 gitoxide（依赖树重、传输认证面尚不成熟）。svn 无进程内选项，本就只有系统客户端一条路。
- Server 端 poll 探测：`scm` 模块（ADR-0009）shell 出 `git ls-remote` / `svn info --show-item revision`，中心化探测，不委托 Agent。
- 前置要求：git ≥ 2.20、svn ≥ 1.10（`--password-from-stdin`），文档化为 Agent 宿主机与 Server 二进制部署的按需前置（不用 checkout 的构建机无需装）；官方 Docker 镜像捆绑 git+svn，Docker 用户零配置。
- 缺二进制 = checkout 步骤/poll 探测清晰报错，不静默降级。

**凭据递送（细节，承 ADR-0015）**：git 走 GIT_ASKPASS；Windows 兼容性不佳时回退临时 credential store 文件（0600、任务毕即删）；svn 用 `--password-from-stdin`。值永不上命令行/URL。

**克隆与增量**

- 首次全量克隆（一次性成本，工作区持久摊销）；浅克隆 v2 候选（与持久工作区的增量 fetch 语义打架、破坏 git log/diff/blame）。
- 增量 git：`fetch origin <分支>` → `checkout --detach` → `reset --hard <sha>` → `clean -fd`（无 `-x`，保忽略文件，ADR-0011 语义）。
- 增量 svn：`svn cleanup` → `svn update -r <rev>`。
- checkout 总是钉到触发时解析出的确切 commit/revision（分支 head 在触发时定，ADR-0006）。

**子模块**：支持，checkout 步骤级开关、默认开（`git submodule update --init --recursive`）；凭据走同一 ASKPASS/credential 机制；不需要的 pipeline 可关。

**分支/修订语义（git 与 svn 差异化）**

- git：项目存「默认分支」设置；手动触发经 `ls-remote --heads` 列分支供选；默认分支缺省在创建项目时解析远端 HEAD 预填。
- svn：无分支概念（URL 即路径），v1 不猜 trunk/branches 布局、不提供分支枚举；手动触发选 revision 号；cron = URL HEAD 最新 revision。

**poll 基线语义**：触发器创建/启用时记录当前 head 作基线、不触发构建（配置变更≠代码变更）；只对之后的新提交触发。

**多仓库**：v1 不做。一个任务 checkout 项目绑定的仓库到工作区根、一个 SCM 上下文；多 checkout 为 v2 候选；辅助仓库用 shell 步骤手动 clone（凭据自担）。

**未来集成留缝**：`VcsType { Git, Svn }` 枚举 + scm 模块内策略分支即可承载；不预立 Provider/Platform 抽象层（webhook 的本质是触发源多样化，SCM 上下文模型已够；过早抽象会猜错未来 API 形状）。

**连通性验证**：项目设置提供可选「测试连接」按钮（ls-remote 验证 URL+凭据、成功返回当前 head），不阻塞保存；poll 探测失败记入触发器历史、继续按节奏重试，不自动禁用触发器。

## 理由

- **shell 出系统客户端**：系统 git 对传输/认证怪癖（smart/dumb http、ssh、代理、LFS）的健壮性无可替代，构建机本就装 git（shell 步骤也要用）；git2/gix 各自的 C 依赖或成熟度问题与 6 目标交叉编译（ADR-0010）相抵。svn 无进程内选项使对称 shell 模型成为最简一致解。
- **bundled sqlx SQLite 先例（ADR-0004）不适用于 libgit2**：sqlx bundling 的是构建链可控的单 C 库；git2 传输层拖的是 openssl/libssh2 整套 TLS/SSH 栈，且系统客户端在用户 shell 步骤里必然存在，进程内实现收益为零。
- **服务端中心化 poll**：poll 是 Server 的触发器职责；委托 Agent 探测把「触发器挂了」的症状从「服务端缺二进制」变成「没有在线 Agent」，更难排查。
- **全量克隆**：Drone/Woodpecker 默认浅克隆的前提是一次性工作区；sisyphus 工作区持久（ADR-0011），一次性全量成本被增量更新摊销。
- **基线不触发**：创建触发器是配置变更；立即跑构建违背「poll = 响应新提交」语义且有副作用风险。
- **svn 不猜布局**：trunk/branches/tags 是约定不是协议，猜错比不提供更糟。

## 后果

- Agent runner 的 checkout 执行器 = 命令编排 + ASKPASS/credential 递送 + 脱敏（ADR-0015）+ 子模块开关；无任何 git/svn Rust 库依赖，agent crate 依赖树保持干净（只依赖 proto，ADR-0009）。
- server `scm` 模块承载 poll 探测 + 连通性验证 + 分支枚举；官方 Docker 镜像（ADR-0010）需加装 git+svn。
- poll 触发器需要「基线 commit」持久字段（首次探测写入）。
- 解锁下游：#13（容器后端--checkout 命令在容器内执行时的前置镜像要求）、#15（web UI--测试连接按钮、分支选择、svn revision 输入）。
- v2 候选：浅克隆、多仓库多 checkout、svn 布局感知、webhook 触发源。

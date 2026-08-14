# 0006 - Pipeline 数据模型与执行语义

日期：2026-08-15
状态：已接受

## 背景

可视化编辑器、调度器、Agent 执行器都吃同一个 Pipeline 数据模型，这是最上游的决策。要对标 Jenkins/bk-ci 的"全 pipeline 形式可视化操作配置"，拓扑表达、参数化、条件执行、失败/重试语义都要在此定板。盘问过程见 [wayfinder 票 #8](https://github.com/Godreach/sisyphus/issues/8)。

## 决策

**拓扑**：阶段串行、阶段内任务并行。不做任务级 DAG，不做跨阶段任务依赖——阶段是人类心智模型（对应 Vue Flow 泳道/分组），DAG 的自由度对 v1 规模收益不抵编辑器"画边+环检测"的复杂度成本。

**三级结构**：
- Pipeline 级：参数定义（string/number/bool/enum 四种、单值、无密码型）、环境变量、通知配置（沿触发器票：pipeline 完成时发送）。
- 阶段级：when 条件。
- 任务级：执行环境（宿主机/容器）、agent 标签要求、when、env（覆盖 pipeline 级同名项）、allow_failure、自动重试次数 retry_count（默认 0）、产物上传路径（声明式，完成后上传）、产物下载依赖（开始前拉取，仅限本次构建内其它任务）。
- 步骤级：仅两种类型——shell（步骤级可选 sh/bash/pwsh/cmd，默认 Unix→sh、Windows→pwsh 无则 cmd）与 checkout scm。产物上/下载不是步骤（改为任务级声明），通知不是步骤（pipeline 完成时发送）。

**参数化**：必填参数必须带默认值（保存时校验），所有触发方式取参一律"默认值，手动触发可覆盖"——不存在 cron/poll 触发缺参数的死锁分支。变量引用 `${name}` 语法（`$${name}` 转义），可用于任意字符串字段；内置变量 8 个（`SISY_BUILD_NUMBER`、`SISY_PIPELINE_NAME`、`SISY_PROJECT_NAME`、`SISY_JOB_NAME`、`SISY_STAGE_NAME`、`SISY_COMMIT_ID`、`SISY_BRANCH`、`SISY_WORKSPACE`），Server 端下发前解析完毕。env 键值对（pipeline/任务两级）与 `${}` 替换是两个机制。

**条件执行**：阶段/任务/步骤三级统一挂 when，语言为受限表达式（比较、`&&`/`||`、字符串相等、存在性），无图灵完备。阶段跳过即其内任务全不发。

**失败与重试**：默认 fail-fast——某任务失败即取消同阶段未完成任务、跳过后续所有阶段；任务可标 allow_failure 豁免。自动重试 N 次后仍失败才算失败。手动重跑两种：从头重跑（新构建占新号）；从失败任务重跑（同号延续，attempt+1，已成功任务的结果/日志/产物保留）。

**构建号**：per-pipeline 自增（#1、#2…），`SISY_BUILD_NUMBER` 取此值。

**并发**：同 pipeline 同时只跑一条构建，后来者 FIFO 排队（与 poll 触发"新提交排队不取消"裁定一致）。

**SCM 源**：手动触发可选分支/commit（默认项目默认分支 HEAD）；cron 触发取默认分支 HEAD；poll 取轮询到的提交。每次构建都有明确的 SCM 上下文。

**版本化**：定义每次保存递增 revision（记操作人与时间）；每次构建入库整份定义快照 JSON（含所用 revision）——可视化编辑随时改定义，快照是"构建 #N 当时到底跑了什么"唯一可靠的依据，SQLite 场景成本可忽略。

## 后果

- 步骤执行器只需实现两种步骤类型，产物传输是任务生命周期的固定环节（后置上传/前置下载），失败时机与重试边界清晰。
- 从失败任务重跑需要 Agent 侧工作区复用配合（同号重跑不重置工作区），工作区隔离/清理策略票因此解锁。
- 跨构建产物引用、参数多值/密码型、任务 DAG 均为 v2 候选，v1 不做。

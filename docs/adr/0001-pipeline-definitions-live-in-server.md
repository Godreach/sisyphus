# 0001 - Pipeline 定义存 Server 端数据库，可视化编辑

日期：2026-08-14
状态：已接受

## 背景

CI 工具的 pipeline 定义存哪有两种主流形态：

1. **repo 内配置文件**（Drone/Woodpecker/GitHub Actions）：`.drone.yml` / `.github/workflows/*.yml` 随代码走，配置即代码，但编辑体验是纯文本，可视化编排（拖拽、连线、状态预览）很难做好。
2. **Server 端数据库存储**（Jenkins 自由风格任务 / 腾讯蓝盾 bk-ci）：定义存在 CI 系统里，web UI 可视化编辑，repo 保持干净；代价是定义脱离代码版本管理，迁移需要导入/导出机制。

sisyphus 对标 Jenkins/bk-ci，产品定义是"全 pipeline 形式的可视化操作配置"——可视化编排是核心卖点，不是附属功能。

## 决策

v1 的 Pipeline 定义只存 Server 端数据库，通过 web UI 可视化编辑，repo 内没有任何配置文件。不做 yaml 导入/导出（v2 视需求再议）。

## 后果

- 可视化编排编辑器成为 web 前端的核心组件，前端选型要优先评估流程图/编排类组件库生态（见前端选型决策票）。
- Pipeline 定义脱离 repo 版本管理；需要用 Server 端的修订历史（版本/审计）部分弥补，v1 至少记录每次编辑的操作人与时间。
- 代码托管平台集成（webhook 触发）推迟到后续版本，v1 触发方式：手动、cron、poll SCM——与"定义不在 repo 里"自洽：repo 里没有文件可供平台解析。
- git 与 svn 作为版本控制客户端能力（checkout、poll）v1 就要有；"关联托管平台"（OAuth、webhook）是另一回事，推迟。

# 0003 - Web 前端选 Vue 3 + Vue Flow

日期：2026-08-15
状态：已接受

## 背景

sisyphus 的核心卖点是可视化 pipeline 编排编辑器（ADR-0001），前端框架选型由"编排组件库生态"决定，而非框架本身。调研（`research/frontend-framework` 分支，issue #2）对比了 React / Vue 3 / Svelte(Kit) 与六个编排组件库（React Flow、Vue Flow、Rete.js、AntV X6、JointJS、Drawflow），数据采集自官方文档、GitHub API 与 npm registry（2026-08-14）。

## 决策

web 前端用 **Vue 3（`<script setup>` + TypeScript + Vite）**，编排编辑器基于 **Vue Flow（`@vue-flow/core`）** 二次封装，i18n 用 **vue-i18n v11**（Composition API 模式）。

> 修订：编辑器形态经 [ADR-0020](0020-web-ui-ia-and-hybrid-editor.md) 调整——Vue Flow 从「编辑器基础」降级为可选依赖，编辑器改用混合式（左数据派生轨道 + 右结构化表单，无画布）；Vue 3 + TS + Vite + vue-i18n v11 不变。

## 理由

- **Vue Flow 是同量级候选中唯一经大规模生产验证的 Vue 3 编排方案**：n8n（editor-ui 直接依赖 @vue-flow/core@1.48.0）与 Kestra 构建于其上；"节点即 Vue 组件"天然承载节点配置表单、校验、只读运行态渲染；MIT 协议、周下载 51 万、2026 年持续发版。
- **中英双语公众产品 + 国内生态**：Element Plus / Naive UI / Arco Design Vue 均活跃维护；对标产品 bk-ci 前端本身是 Vue。
- **React + React Flow 完全成立**（React Flow 是编排库标杆，38k stars、xyflow 全职团队维护），但项目方对框架无偏好时，Vue 3 在本场景无短板，响应式模型对 pipeline 双向绑定/表单联动更省事；若未来需求极端复杂化，两者概念模型同源，迁移路径存在。

## 后果

- **Vue Flow 为个人主导项目**（非全职团队），接受"社区依赖"风险：锁版本 + fork 兜底；React Flow 是逃生通道。
- Svelte(Kit) 排除：生态最薄（Svelte Flow 2024 年才发布，周边、组件库、资料明显不足）。
- i18n 的 key 拼写错误不在编译期暴露，双语完整性靠 CI 对账脚本（zh-CN.json / en-US.json catalog）。
- 前端放 monorepo 子目录（`web/`），Vite 构建产物由 Rust server 内嵌静态伺服（与发布形态票 #16 衔接）。

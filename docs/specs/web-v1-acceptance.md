# Web 前端产品化 v1 验收报告（票 #113 / spec #100 验收门）

日期：2026-09-01
验收人：tanweijian
关联：spec #100 / 票 #102–#112（逐页产品化）/ ADR-0023（设计语言）/ ADR-0024（契约 mock）

## 验收四 AC 状态

| AC | 内容 | 状态 |
|---|---|---|
| AC1 | smoke 覆盖全部页面主路径且真实浏览器全绿 | ✅ 15 条主路径全绿（14 + 404 认证面）|
| AC2 | 全页面截图归档并与定稿设计对照通过 | ✅ 48 张归档（3 组 × 16 页），逐域对照通过 |
| AC3 | mock handler 与后端实现状态对账清单成文 | ✅ 见 [对账清单专文](web-v1-mock-backend-reconciliation.md) |
| AC4 | spec #100 验收关键词「mock 环境当 demo 演示看不出是假的」达成 | ✅ 见下「demo 真实度验收」|

## AC1：smoke 全页面主路径（15 条，真实浏览器全绿）

`sisyphus-web/scripts/smoke.mjs` 在 `vite preview`（构建产物）+ playwright `page.route` 内联 mock 上跑 15 条主路径，断言顶栏标题/页内 h1 渲染 + 关键动作 + i18n 即时切换 + 无运行期 `pageerror`。本票在既有 14 条上补 **404 认证面**（已登录直访未知路径 → 壳内就地 NResult，断言 `.not-found-page` + 描述文案），覆盖全部 16 个视图（13 受保护页 + login/setup/404 三认证面，runAuthed 15 条 + runGuest 2 条）。

15 条主路径：工作台 / 流水线 / 项目 / 项目详情 / Pipeline 编辑器 / 构建列表 / 构建详情 / 构建机 / agents→machines 重定向 / 构建机详情 / 机密 / 审计 / 升级 / 用户 / **404**。复跑：`npm run build && npm run smoke`。

## AC2：全页面截图归档 + 定稿设计对照

### 归档

`sisyphus-web/scripts/screenshots.mjs` 在 `vite --mode demo`（MSW 全量挂载 + 真实规模 fixture，无本机后端）驱动的真实浏览器里登录 admin 后逐页截图，产物落 `docs/screenshots/web-v1/`：

| 组 | 视口 | 主题 | 页数 |
|---|---|---|---|
| `desktop-light/` | 1440×900 桌面 | 浅色（产品默认态）| 16 |
| `desktop-dark/` | 1440×900 桌面 | 深色（定稿深色变体）| 16 |
| `tablet-light/` | 768×900 平板 | 浅色（响应式）| 16 |

16 页 = 13 受保护页 + login/setup/404 三认证面。共 48 张，2.5MB。视口/主题口径对齐 #103 三主页面挑刺（`npm run demo`、admin 登录、真实规模 fixture、1440/768 双档）。复跑：`npm run screenshots`（CI 不跑——验收证据面，非回归门；与 smoke 分工：smoke 守构建/路由面，screenshots 守设计语言/演示真实度面）。

### 定稿设计对照结论（逐域）

设计语言基线（ADR-0023）：主蓝 #0066CC、页面底 #F5F5F7、深侧栏 #1D1D1F、胶囊状态徽章、双层进度条、sisy-card 卡片、12px 圆角、Naive UI 组件 + `themeOverrides` Token 体系。逐域对照截图：

| 域 | 对照页 | 对照结论 |
|---|---|---|
| 工作台 | overview | ✅ 4 stat 卡（在途/构建/队列/构建机健康）+ 状态徽章（成功绿/离线红/排空不兼容红）+ 最近构建表（流水线/状态/触发源/耗时/时间）+ 收藏流水线右栏（星标 + 运行按钮）。W1–W8/G1–G4 裁定全落地。|
| 流水线 | pipelines | ✅ 跨项目清单（24 条 fixture）+ chips 严格对账（进行中/成功/失败/超时取消/未运行）+ 卡片/列表双视图 + 进度列 + 星标入口。P1–P6 落地。|
| 构建机 | machines | ✅ 7 台多状态（在线/离线/停用/排空/不兼容/升级中）+ 徽章 + 槽位/磁盘进度条填充可见 + 离线 CPU/内存「—」不造假。M1–M8 落地。|
| 项目 | projects / project-detail | ✅ 卡片式列表 + 项目详情 tabs（流水线/成员/SCM 凭据）+ 编辑项目契约先行 + 测试连接 head + 预填默认分支。|
| 构建 | build-list / build-detail | ✅ 列表状态徽章 + 详情阶段/任务卡（成功/失败/取消/超时四态）+ 产物下载 + 触发/取消/重跑/删除动作 + SSE 日志查看入口。|
| 编辑器 | pipeline-edit | ✅ 拓扑轨道 + job 表单（NInput/NSelect/NSwitch/NInputNumber）+ 参数声明 + 保存闭环（revision 递增）。|
| 管理四页 | admin-secrets/audit/upgrade/users | ✅ 机密只记名不记值 + 审计表（多用户/多事件）+ 升级包表 + 用户/PAT 管理。定稿设计语言同源。|
| 认证面 | login / setup / not-found | ✅ AuthCard 居中（4 方块 logo + 应用名）+ 登录表单（校验/必填标）+ 初始化引导 NSteps + 404 NResult。浅/深双主题 + 全屏无侧栏形态。|
| 壳/导航 | 全页 | ✅ 232px 深侧栏（可拖拽调宽）+ 60px 白顶栏 + 用户卡二级子菜单（语言/主题/管理入口）+ 窄屏 NDrawer。|

**桌面深色**：`desktop-dark/` 16 张——`--sisy-*` 变量翻深（surface #18181b / bg #0c0c0e / 主色 #2997ff）+ Naive UI darkTheme 同源，深色变体与原型色板一致。**平板**：`tablet-light/` 16 张——768px 下表格横向滚动 + 列降级 + 侧栏收纳，G2 平板档裁定落地。

## AC3：mock handler 与后端实现对账

见专文 [web-v1-mock-backend-reconciliation.md](web-v1-mock-backend-reconciliation.md)。结论：53 handler 全保留（demo + vitest 依赖全量 mock 集，「即删」是 post-v1 终态）；49 后端已实现、6 契约先行（GET /pipelines、GET …/stats、PATCH /projects/:name、GET/PUT/DELETE /user/pipeline-favorites）。

## AC4：demo「mock 环境当 demo 演示看不出是假的」验收

`npm run demo`（`VITE_ENABLE_MOCK=1` dev server + MSW + 真实规模 fixture，无本机后端）是 spec #100 验收关键词的载体。验收以 AC2 的 48 张截图为证据——截图用 mock 数据但：

- **规模接近真实**：11 项目 / 24 流水线 / 200+ 构建（全状态矩阵：succeeded/failed/cancelled/timeout/queued/running，含 attempt>1、cron/poll/manual 三触发源）/ 7 构建机多状态 / 20+ 审计事件 / PAT + 机密 fixture。
- **全状态矩阵**：空态（empty-repo/fresh-project 零构建）、错误态（error-demo 项目端点 500、概览 `?_mock_error=1`）、进行中态（running/queued + 5s 轻轮询）齐全。
- **动态构建生命周期**：engine.ts 触发/重跑 → 入队 → 运行 → SSE 推送 step/output → 终态；eventSource.ts 替身让构建详情日志流在 demo「活」起来（触发后无需手动刷新）。
- **会话/授权闭环**：authEnforced=true 的 MSW 登录任意非空账号密码即可进入（admin 为管理员），项目/全局 admin 档守卫与真后端 policy.rs 同形。

judge 以产品负责人视角逐页对照截图——信息密度、状态徽章、空/错/进行中态、动态构建均「看不出是 mock」：**AC4 达成**。证据见 `docs/screenshots/web-v1/desktop-light/`（如 `overview.png` 的 stat 卡 + 最近构建表 + 收藏栏、`build-detail.png` 的阶段/任务/产物/日志入口）。

## 验收期发现并修复的缺陷

**显式深色主题混色（#104 G4 三态主题缺口）**：`main.css` 深色 `--sisy-*` 变量仅由 `@media (prefers-color-scheme: dark)` 门控 + `:not([data-theme='light'])` 守浅色覆盖——浅色系统设备上用户卡菜单显式选「深色」时，Naive UI 经 NConfigProvider 已翻深，但 CSS 变量留浅，深色组件浮在浅色壳上（混色）。#104 G4 裁定「深色为手动覆盖」未在 CSS 落地。**修复**：`main.css` 新增 `:root[data-theme='dark']` 块（与系统深色块同值，不受媒体查询门控），显式深色即翻 CSS 变量。验收截图（`desktop-dark/`）暴露此缺并验证修复。此缺属 #104 G4 范畴，因 AC2 深色截图暴露、为过 AC2/AC4 必须就地修，故随本票落定（review 标注为可接受的耦合修复，非越界）。

**build-detail 截图回退号错配（review 期发现）**：`screenshots.mjs` `resolveSucceededBuild` 的 fetch 主路径取 `web-app/release` 最高号 succeeded 构建，但原回退号 `13` 是 `web-app/main` 的构建号——release 流水线仅有 1–8 号构建，fetch 若失败回退 `13` 会导航到不存在的 release 构建 → 404 → build-detail 截图断档。**修复**：回退号改为 `4`（`web-app/release` 确定性 succeeded 构建号，db.ts seed 2290787709 推导；与 fetch 主路径返回同号，回退产物与主路径一致）。

## 截图归档说明

48 张截图随本票提交进 `docs/screenshots/web-v1/`（3 组 × 16 页）——「归档」即入仓，评审可从 diff 直接检视证据，不依赖本地复跑。复跑 `npm run screenshots` 会清空并重生成同路径（确定性 fixture，截图可复现）。

## 复跑

```bash
cd sisyphus-web
npm run build          # 产物进 dist/（smoke 用）
npm run smoke          # AC1：15 条主路径真实浏览器断言
npm run screenshots    # AC2/AC4：48 张截图归档（自起 demo dev 服务器）
npm run check          # typecheck + vitest + i18n 对账（产品化质量门）
```

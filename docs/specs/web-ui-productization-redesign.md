# Web UI 产品化改造

## Problem Statement

sisyphus v1 的 web UI 是功能原型阶段，视觉设计、组件体系、交互体验、信息架构、响应式适配均未达到产品化 release 标准。具体表现为：

- 无统一组件库，2645 行手写 CSS 维护成本高且各页面风格不一致
- 缺少 loading 状态、错误反馈、toast 通知等交互细节
- 侧栏导航仅有纯文字链接，无图标、无分组、无 hover 效果
- 无 dark mode 支持
- 仅适配桌面端，平板设备体验差

## Solution

采用渐进式迁移策略，引入 Naive UI 组件库，从 Login/Setup 页面开始逐页面改造。通过 Naive UI 的 `themeOverrides` 机制统一设计 Token，参考 bk-ci 的卡片式布局、表格设计和状态标签系统，结合 GitHub 极简风的视觉基调，将 web UI 提升至产品化水平。

## User Stories

### 基础设施

1. As a developer, I want Naive UI integrated into the Vite build pipeline, so that all pages can use production-grade components
2. As a developer, I want a centralized theme configuration via Naive UI's `themeOverrides`, so that colors, spacing, and typography are consistent across all pages
3. As a developer, I want dark mode to follow the system preference (`prefers-color-scheme`), so that users get appropriate contrast without manual switching
4. As a developer, I want the sidebar redesigned with icons, group titles, and hover effects, so that navigation feels professional and organized
5. As a developer, I want the app shell to be responsive down to 768px (tablet), so that users can check build status on iPad

### Login / Setup 页面（首个迁移锚点）

6. As a user, I want a visually polished login page with a centered card layout, so that the first impression of sisyphus feels professional
7. As a user, I want form validation feedback (inline errors, input highlights) on the login form, so that I can correct mistakes without guessing
8. As a user, I want a loading spinner during login submission, so that I know the system is processing
9. As a user, I want the setup wizard to use Naive UI's steps component, so that progress is visually clear
10. As a user, I want the setup wizard's CLI command display to use a code block component with copy button, so that copying commands is effortless
11. As a user, I want toast notifications for setup actions (create admin, register agent, create project), so that I get immediate feedback
12. As a user, I want the 404 page to use the new design language, so that even error pages feel cohesive

### Overview 概览页

13. As a user, I want stat cards using Naive UI's NCard component with clear visual hierarchy, so that key metrics are scannable at a glance
14. As a user, I want the queue reason classification displayed with color-coded status tags, so that I can quickly identify bottleneck types
15. As a user, I want alert states (agent offline, no matching agent) to use Naive UI's alert component, so that warnings are visually prominent
16. As a user, I want the recent builds table to use Naive UI's NDataTable with sortable columns, so that I can find builds efficiently
17. As a user, I want the overview page to show a skeleton loader while data is fetching, so that the page doesn't feel empty during load

### Projects 页面

18. As a user, I want the project list to use card-based layout, so that each project is visually distinct
19. As a user, I want the project create form to use Naive UI form components with validation, so that SCM URL and credential inputs are validated before submission
20. As a user, I want a test-connection probe with visual feedback (spinner → success/failure badge), so that I can verify SCM connectivity without leaving the form
21. As a user, I want project detail pages to use Naive UI's tabs for pipeline list, members, and SCM credentials, so that information is organized without clutter

### Pipeline Editor

22. As a user, I want the pipeline editor's topology track to use Naive UI's tag/chip components for job nodes, so that the visual style matches the rest of the app
23. As a user, I want the job form panel to use Naive UI's form components (NInput, NSelect, NSwitch, NInputNumber), so that form interactions are consistent
24. As a user, I want pipeline validation errors to display using Naive UI's form validation patterns, so that error messages are visually integrated
25. As a user, I want the pipeline editor's concurrent save conflict dialog to use Naive UI's modal, so that conflict resolution feels native

### Build 页面

26. As a user, I want the build list to use NDataTable with status filter dropdowns, so that I can quickly find builds by status
27. As a user, I want build status indicators to use a consistent color-coded tag system (success=green, failure=red, running=blue, cancelled=gray), so that status is instantly recognizable
28. As a user, I want the build detail page's stage/job cards to use Naive UI's NCard with status badges, so that build progress is visually clear
29. As a user, I want the trigger dialog to use Naive UI's modal with form components for parameter override, so that triggering builds feels polished
30. As a user, I want the build log viewer to maintain its existing SSE streaming behavior but with improved visual styling (monospace font, step collapsibles, ANSI color support), so that log reading is comfortable
31. As a user, I want artifact download links to use Naive UI's button component with download icons, so that the action is visually clear

### Agent 页面

32. As a user, I want the agent list to display agent status using color-coded badges (online=green, offline=red, draining=yellow, incompatible=gray), so that fleet health is scannable
33. As a user, I want the agent create dialog to use Naive UI's modal with one-time token display in a code block, so that registration is straightforward
34. As a user, I want agent detail pages to use Naive UI's description list for system/custom labels, so that agent metadata is organized
35. As a user, I want workspace and cache management actions to use Naive UI's confirmation dialogs, so that destructive operations require explicit consent

### Admin 页面

36. As an admin, I want the secrets management page to use Naive UI's list component with write-only value display, so that secret names are clearly listed
37. As an admin, I want the audit log to use NDataTable with time range filters and pagination, so that security events are searchable
38. As an admin, I want the agent upgrade page to use Naive UI's upload component and progress indicators, so that upgrade operations are visually tracked
39. As an admin, I want the user management page to use Naive UI's form components for CRUD operations, so that user administration is consistent with other forms

### 导航与全局

40. As a user, I want the sidebar to show icons next to each navigation item, so that pages are visually distinguishable
41. As a user, I want the sidebar to group nav items with section titles (Main / Admin), so that the navigation hierarchy is clear
42. As a user, I want the sidebar to highlight the current page with a colored background and icon, so that I always know where I am
43. As a user, I want the footer to show my username and a logout button using Naive UI components, so that session management is accessible
44. As a user, I want the language toggle to use Naive UI's switch or segmented control, so that it matches the overall design language

### 响应式

45. As a user on a tablet, I want the sidebar to collapse into a hamburger menu on screens narrower than 768px, so that content area has more space
46. As a user on a tablet, I want tables to horizontally scroll on narrow viewports, so that data is accessible without breaking layout
47. As a user on a tablet, I want forms to stack vertically on narrow viewports, so that input fields are usable on touch screens

### 交互细节

48. As a user, I want all data-fetching pages to show skeleton loaders during initial load, so that the app feels responsive
49. As a user, I want form submissions to show loading states on submit buttons, so that I don't double-submit
50. As a user, I want destructive actions (delete project, delete build, revoke token) to show confirmation dialogs, so that I don't accidentally lose data
51. As a user, I want success/error toast notifications after CRUD operations, so that I get immediate feedback without navigating away
52. As a user, I want empty states (no projects, no builds, no agents) to show helpful illustrations and call-to-action buttons, so that I know what to do next

## Implementation Decisions

### 模块结构

- 新增 `src/composables/useNaive.ts`：集中 Naive UI 的按需导入配置（`create`、`NConfigProvider`）
- 新增 `src/theme/index.ts`：导出 Naive UI `themeOverrides` 配置对象，包含颜色、字体、圆角、阴影等 Token
- 新增 `src/components/base/`：放置跨页面复用的基础组件（StatusBadge、EmptyState、SkeletonLoader 等）
- `src/assets/main.css` 逐步精简，最终仅保留 Naive UI 无法覆盖的布局样式（如 `.app-shell` 布局）

### App Shell 改造

- `App.vue` 的 `<aside>` 侧栏改用 Naive UI 的 `NMenu` 组件，配置 `render-label` 插槽支持图标
- 侧栏图标从 `@vicons/ionicons5` 导入（Naive UI 推荐搭配）
- 侧栏分组通过 `NMenu` 的 `group` 选项实现
- 侧栏宽度从固定 180px 改为可响应：桌面端 200px，平板端可折叠

### 主题配置

- Naive UI `themeOverrides` 中配置：
  - 主色调：`primaryColor` 使用 GitHub 风格的蓝色（`#2b5797` 保持现有）
  - 成功/失败/警告色：参考 bk-ci 的状态色体系
  - 圆角：保持 `6px` 与现有设计一致
  - 字体：保持现有字体栈
- Dark mode 通过 `NConfigProvider` 的 `theme` 属性切换，监听 `window.matchMedia('(prefers-color-scheme: dark)')` 自动切换

### 响应式策略

- 使用 CSS 媒体查询 `@media (max-width: 768px)` 处理平板适配
- 侧栏在窄屏下使用 Naive UI 的 `NDrawer` 组件抽屉式展开
- 表格使用 Naive UI `NDataTable` 内置的 `scroll.x` 横向滚动
- 表单使用 CSS Grid/Flexbox 自动堆叠

### 迁移策略

- 每迁移一个页面，在该页面的 `<style>` 块中移除对 `main.css` 中对应规则的依赖
- 迁移完成后，删除 `main.css` 中已无引用的 CSS 变量和规则
- 新组件统一从 `@/components/base/` 引用，不新增全局 CSS 类

## Testing Decisions

### 测试原则

- 只测试外部行为（渲染、交互、API 调用），不测试 Naive UI 组件内部实现
- 组件测试使用 `@vue/test-utils` 的 `mount`/`shallowMount`，mock Naive UI 组件为简单 stub
- 集成测试验证页面级行为：表单提交、导航跳转、状态显示

### 测试范围

- `src/composables/__tests__/useNaive.test.ts`：验证主题配置是否正确导出
- `src/components/base/__tests__/`：每个基础组件的单元测试
- `src/views/__tests__/`：每个迁移后页面的组件测试
- 现有 Vitest + jsdom + vue-test-utils 测试基础设施保持不变

### 先例

- 现有 `src/stores/__tests__/` 和 `src/api/__tests__/` 目录已有测试模式可参考
- Playwright smoke test 可扩展覆盖迁移后的关键页面

## Out of Scope

- Pipeline Editor 的交互逻辑增强（拖拽排序、撤销/重做等）— 保持现有交互，只换视觉
- 移动端（< 768px）适配 — v1 仅覆盖桌面 + 平板
- 自定义组件库构建 — 完全依赖 Naive UI
- 新的业务功能开发 — 本次仅做 UI 改造
- ADR-0020 的修改 — 已创建独立的 ADR-0023 记录本次决策

## Further Notes

- ADR-0023 已创建于 `docs/adr/0023-web-ui-productization-redesign.md`
- 迁移路线图将在首个锚点页面（Login/Setup）完成后制定完整版本
- 渐进式迁移允许与新功能开发并行，约定：修改页面时顺带迁移到新组件
- Naive UI 的按需导入通过 `unplugin-vue-components` + `unplugin-auto-import` 配置，避免全量引入增大包体积

# sisyphus web UI 原型（一次性）

> **PROTOTYPE - throwaway.** 票 #15（决策：web UI 信息架构与原型验证）的原型产物。
> 只存在于 `prototype/web-ui-ia` 分支，永不合入 main。数据全部虚构。

## 运行

```bash
cd web
npm install
npm run dev        # http://localhost:5199
```

生产构建 + headless 冒烟（可选，需本机 Chrome）：

```bash
npm run build && npm run preview   # http://localhost:4173
node scripts/smoke.mjs             # 无头点击冒烟（对 preview 端口 5299）
```

## 看什么

侧栏可切 中文/EN（vue-i18n v11，zh 为源语言）。页面：

- `#/overview` — 概览：stat 卡 + 事实型警示态（ADR-0019）+ 最近构建
- `#/projects` — 项目列表 + 新建（测试连接 / ls-remote 分支预填 / SCM 凭据，ADR-0016）
- `#/builds/b1` — 构建详情：阶段/任务卡（含「缺失标签」等待态）、SSE 日志流（步骤生命周期+输出交织）、产物
- `#/agents` / `#/agents/a1` — 四态 Agent（在线/离线/排空/不兼容）、系统/自定义标签、槽位、磁盘占用、工作区/缓存清理
- `#/admin/secrets|audit|upgrade|users` — 机密（只记名）、审计、Agent 升级（包上传+排空）、用户/PAT
- `#/setup` — 初始化引导：三步各自可跳过 + CLI 等价提示

## Pipeline 编辑器 — 三个结构不同的变体

编辑页底部悬浮条切换（←/→ 或方向键），URL 带 `?variant=A|B|C`：

| 变体 | 形态 | 要点 |
|---|---|---|
| **A 画布拖拽** | Vue Flow 全画布 | 阶段=列、任务=可拖节点、连线自动生成；点击节点开右侧属性抽屉 |
| **B 结构化表单** | 树形大纲 + 详情表单，零画布 | 任务/参数/环境变量三个页签；每个属性都是显式字段 |
| **C 混合** | 派生式小地图栏 + 表单为主 | 左侧只读「轨道」始终显示整条 pipeline 拓扑，点击导航表单；布局由数据派生、不可手拖 |

## 冒烟结论

`scripts/smoke.mjs`（headless Chrome）：向导步进、i18n 切换、JSON 折叠、大纲选任务、
变体切换（按钮+键盘）、三变体渲染、admin 各页全部通过。
内嵌浏览器（IAB）中部分按钮点击不触发是其渲染怪癖，非应用缺陷。

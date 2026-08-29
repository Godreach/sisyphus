# sisyphus-web

sisyphus 的 Web 前端工程（ADR-0003：Vue 3 + TypeScript + Vite；ADR-0020：
12 页 IA + 混合式编辑器）。构建产物输出到 `dist/`——sisyphus-server 经
rust-embed 内嵌该目录产物对外提供静态服务与 SPA fallback（release 编译期
嵌入、debug 运行时读盘，ADR-0005），server 侧零改动。

本地覆盖目录（同名文件压过内嵌资源）在 Server 数据目录的 `web/` 子目录。

## 目录约定

- `src/api/`：单实例 API 客户端——`http.ts`（fetch 核心：cookie 会话 + 可选
  Bearer PAT 双通道、401 统一落登录、统一错误形态 `code`/`message`/`detail`
  按 code 分支、校验清单 `detail.errors` 按字段路径定位）+ `http-singleton.ts`
  （模块级单实例）+ `client.ts`（端点级封装：auth/setup）+ `types.ts`
  （错误形态类型）。
- `src/model/`：sisyphus-model 生成产物落点（ADR-0009，B4-T7 建立生成/对账
  管线；本批次为空目录，类型以 API 客户端 DTO 先行）。
- `src/i18n/`：zh 源语言 + en 全量对译 catalog（`locales/`）+ 装配
  （`index.ts`）。key 集合一致性由 `npm run i18n:check` 强制（CI 挂）。
- `src/stores/`：Pinia store（auth 等）。
- `src/views/`：12 页 IA + 登录 + 初始化引导（页面实现归各页面票）。
- `src/components/`：页面组件（侧栏、轨道、表单、日志流等，页面票落地）。
- `src/router/`：路由表（ADR-0020 12 页 IA）+ 守卫（会话恢复 `/auth/me`
  锚点、未认证重定向登录 + 回跳、`/setup` 引导位）。
- `src/mocks/`：MSW 契约 mock 底座（ADR-0024，票 #101）——`db.ts`（确定性
  fixture：真实规模项目/流水线/构建/Agent + 空态/错误态钩子）、`engine.ts`
  （动态构建生命周期：触发/重跑排队 → 运行 → 步骤/输出推送 → 终态）、
  `handlers.ts`（核心链路 REST handlers，dev worker 与 vitest node 共用）、
  `eventSource.ts`（SSE 日志流替身——MSW 拦不到 EventSource）。后端每就绪
  一个端点即删除对应 handler，mock 层随后端进度收敛为零。

## 依赖纪律

依赖锁版本（`package-lock.json` 提交，CI `npm ci` 按锁文件安装）。
**不引入 Vue Flow**（ADR-0020：编辑器不是画布；无消费者则不装）。

## 脚本

- `npm run dev`：Vite dev server（`/api` 代理到本机 server 默认 8080 端口，
  开发期前后端同源，cookie 会话与 CSRF 语义一致）。`VITE_ENABLE_MOCK=1`
  时改为挂载 MSW worker + SSE 日志流替身，全应用在 mock 数据上可用
  （登录账号 `admin/admin123`，见 `src/mocks/`）；关闭开关（缺省）走
  proxy 连真后端，行为与现状一致。handler/fixture 代码经动态 import +
  `import.meta.env.DEV` 守卫不进生产产物；`public/mockServiceWorker.js`
  会被原样复制进 `dist/`，但生产无 `setupWorker` 注册，纯惰性文件无行为。
- `npm run build`：vue-tsc 类型 + vite 构建，产物进 `dist/`。
- `npm run preview`：预览构建产物。
- `npm run typecheck` / `npm test` / `npm run i18n:check`：CI 三件套
  （vue-tsc 类型 + vitest 行为测试 + i18n 对账）。
- `npm run smoke`：headless 冒烟（票 B4-T9）——`vite preview` 伺服 `dist/`，
  playwright 在真实浏览器里走通 12 页主路径 + i18n 切换 + 登录/引导公开页。
  后端经 playwright `page.route` 拦截 `/api/v1/**` 注入 mock（不拉真后端）；
  数据往返由 Rust `web_handshake.rs` 进程内 oneshot 兜底。本地跳过 chromium
  下载可用 `SMOKE_CHROMIUM_EXECUTABLE` 指向系统 Chrome/Edge。CI 在 `build`
  后 `npx playwright install --with-deps chromium` 再跑（见 `.github/workflows/ci.yml`）。

## 测试纪律

Vitest + Vue Test Utils，只测外部行为（用户可见状态、DOM 事件、网络请求/
响应形态断言），不测组件内部结构。组件挂载测试的驱动缝按 ADR-0024 收敛：
经真实 http client 打 MSW node handlers（`src/mocks/node.ts`，fixture 即
测试数据），新 spec 优先走该缝，逐票淘汰手写 fetch mock。
headless 冒烟补 jsdom 测不到的构建/路由/历史 API 面（真实浏览器驱动）。

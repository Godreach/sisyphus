# 前后端 mock 接口对账基线（票 #144）

日期：2026-09-19
关联：ADR-0024 契约 mock 开发模式 / spec #100 AC3 / 票 #134—#143

## 当前结论

本清单以 `sisyphus-server/src/api/mod.rs` 的 Axum 组合根为 server 路由事实源，同时校验 `sisyphus-server/tests/snapshots/openapi.json`；以运行时 `createHandlers()` 暴露的 MSW method/path 为 mock 事实源。动态路径参数名统一为 `:param` 后比较；参数所在分段、静态路径和 HTTP method 仍须完全一致。

| 对账项 | 数量 | 结论 |
|---|---:|---|
| server Axum 路由 | 96 | 已纳入版本化基线；其中 92 个进入 OpenAPI snapshot |
| 前端 MSW handler | 71 | 已纳入版本化基线 |
| handler 有 server 对应 | 71 | 全部对应 |
| handler 对应的 server 缺失 | 0 | 票 #134—#143 已补齐历史缺口 |
| server 已有、mock 未补 | 25 | 均为下述刻意排除项，无未登记漂移 |

完整的逐端点基线在 `sisyphus-web/src/mocks/apiInventory.ts`。它把端点分成 `sharedApiEndpoints`（71 个，两边共有）和 `serverOnlyApiEndpoints`（25 个，仅 server）；其中 `serverRoutesOutsideOpenApi` 记录 3 个 Agent 日志归档上传端点与 `/metrics`。比在文档中复制 96 行更适合作为可执行的单一维护入口。

## 为什么 demo/test 模式仍保留全量浏览器 mock

ADR-0024 的长期方向是后端就绪后收敛 mock，但当前 demo 与隔离前端测试仍需要 71 个浏览器消费端点全部可用：

- `npm run dev`：默认不启动 MSW，`/api` 由 Vite proxy 转发到真实 server；这是日常真实后端模式。
- `npm run demo`：通过 `VITE_ENABLE_MOCK=1` 启动 MSW worker，不要求本机 server；用于产品演示和设计验收。
- vitest：通过 MSW node server 挂载同一套 handlers，组件经真实 HTTP client 验证契约，不为每个测试另写 fetch stub。
- production build：`import.meta.env.DEV` 守卫使其始终不启用 MSW，即使设置 mock 开关也不会进入 mock 模式。

因此“真实后端已实现”不再等于“立即删除 handler”。删除某个 shared handler 必须同时意味着 demo/test 不再需要该浏览器契约，或 demo 已迁移到真实 server + seed；否则会破坏离线演示与组件测试。

## server 已有但 mock 未补（25 个）

这些端点不属于当前浏览器 demo/test 的消费面，所以明确保留在 `serverOnlyApiEndpoints`，不是遗漏。

| 类别 | 数量 | 端点范围 | 不 mock 的原因 |
|---|---:|---|---|
| Agent 协议面 | 13 | `/api/v1/agent/register`、Agent 产物与日志归档上传/下载/发布、升级包下载 | Agent 进程消费，不由浏览器调用 |
| 触发器管理 | 4 | pipelines 下 triggers 的 GET/POST/PATCH | v1 前端尚无触发器管理 UI |
| 日志正文 | 2 | attempt logs 下载与 SSE stream | demo 走 `eventSource.ts` 替身；REST handler 只覆盖归档状态 |
| SMTP 配置 | 2 | `GET/PUT /api/v1/config/smtp` | v1 前端尚无 SMTP 配置 UI |
| 认证自助面 | 2 | `POST /api/v1/auth/register`、`POST /api/v1/auth/password` | v1 前端无自助注册/改密 UI |
| 运维探针 | 2 | `GET /healthz`、`GET /metrics` | 运维消费，不属于 SPA API |

## 可执行防漂移检查

在 `sisyphus-web/` 运行：

```bash
npm run api:check
```

该命令会：

1. 从 Axum 组合根提取全部 server method/path，并从 committed OpenAPI snapshot 提取公开契约子集；
2. 实例化 `createHandlers()` 提取真实 MSW method/path；
3. 分别与完整 server 基线、OpenAPI 子集基线和 shared mock 基线比较；
4. 失败时逐条输出“缺失（基线有、实际无）”和“新增（实际有、基线无）”。

该 spec 会被前端 `npm test` 自动发现，因此 `npm run check` 和 GitHub Actions 的 `frontend` job 都运行它；`npm run api:check` 供快速单独复跑。server 源码与 OpenAPI snapshot 的生成一致性继续由 `sisyphus-server/tests/openapi_snapshot.rs` 守护；两道门组合后，server 路由或 mock handler 的新增、删除、method/path 变化都会在质量门中显式出现。

## 有意变更接口时

1. 更新 server 路由/utoipa 注解或 MSW handler；若 server 契约变化，按 server README 重写并评审 OpenAPI snapshot。
2. 运行 `npm run api:check`，阅读具体差异，确认对应任务票及端点应属于 shared 还是 server-only。
3. 更新 `apiInventory.ts` 的相应基线，并同步本文数量/分类。
4. 运行 `npm run check`、相关 server 测试和既有 demo/smoke 验证。

不要为了让检查变绿而自动重写基线；基线变化本身就是接口评审面。

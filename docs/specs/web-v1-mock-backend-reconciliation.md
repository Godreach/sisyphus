# mock handler 与后端实现对账清单（票 #113 AC3）

日期：2026-09-01
关联：spec #100 验收门 / ADR-0024 契约 mock 开发模式 / 票 #102/#104/#105/#108

## 目的

spec #100 整体验收门的 AC3：mock handler 与后端实现状态对账清单成文——逐 handler 标注后端是否已实现（已切真 / 待后端），并回答「后端已实现端点的 handler 是否已删」。

## 对账口径（ADR-0024 handler 生命周期）

ADR-0024 定的策略是 **handler 生命周期 = 后端就绪即删**：后端每实现一个端点，对应 handler 删除，`npm run dev`（proxy 连真后端）路径自动落到真后端，mock 层随后端进度**收敛为零**。

**v1 现状：收敛未启动——53 个 handler 全保留。** 这是有意偏离「即删」终态，根因是 demo 模式（`npm run demo`，`VITE_ENABLE_MOCK=1`，无本机后端）与 vitest node 模式都依赖**全量 handler 集**才能跑：

- `npm run dev`（mock 关闭）→ `/api` proxy 到真后端，handler 不挂载——此路径已切真。
- `npm run demo`（mock 开）→ MSW worker 挂全量 handler，无后端——demo 是 spec #100 验收关键词「mock 环境当 demo 演示看不出是假的」的主载体，其存在要求全量 handler。
- vitest → MSW node 模式挂全量 handler——组件挂载测试经真实 http client 打 MSW。

故「即删」是 demo 退役（或改跑真后端 + seed）后的 **post-v1 终态**，v1 期 handler 全保留。本对账的**可执行输出**是后端仍欠的契约先行端点（见「契约先行清单」），而非删 handler。

## 对账总表（53 handler，按域分组）

状态列：✓ 后端已实现（dev 模式已切真，demo/vitest 仍走 mock）｜✗ 契约先行（后端待实现）。

### 认证 / 会话（auth.rs / tokens.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| POST | /auth/login | login | auth.rs:218 | ✓ |
| POST | /auth/logout | logout | auth.rs:306 | ✓ |
| GET | /auth/me | me | auth.rs:340 | ✓ |
| POST | /auth/setup | setup | auth.rs:106 | ✓ |
| GET | /auth/tokens | tokensList | tokens.rs:156 | ✓ |
| POST | /auth/tokens | tokenCreate | tokens.rs:89 | ✓ |
| DELETE | /auth/tokens/:id | tokenRevoke | tokens.rs:176 | ✓ |

### 概览 / 收藏（overview.rs；favorites 后端整域缺失）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | /overview | overview | overview.rs:112 | ✓ |
| GET | /user/pipeline-favorites | favoritesList | — | ✗（#104 契约先行，favorites 后端整域不存在）|
| PUT | /user/pipeline-favorites/:project/:pipeline | favoriteAdd | — | ✗（同上）|
| DELETE | /user/pipeline-favorites/:project/:pipeline | favoriteRemove | — | ✗（同上）|

### 项目 / 成员 / SCM（projects.rs / members.rs / scm.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | /projects | projectsList | projects.rs:114 | ✓ |
| POST | /projects | projectCreate | projects.rs:139 | ✓ |
| GET | /projects/:name | projectGet | projects.rs:231 | ✓ |
| PATCH | /projects/:name | projectPatch | — | ✗（#108 契约先行，后端有意缺口：项目编辑级联语义未裁定）|
| GET | /projects/:name/members | projectMembersList | members.rs:97 | ✓ |
| PUT | /projects/:name/members | projectMembersReplace | members.rs:121 | ✓ |
| GET | /users/directory | usersDirectory | users.rs:270 | ✓ |
| PUT | /projects/:name/scm-credential | projectScmCredential | scm.rs:220 | ✓ |
| POST | /projects/:name/test-connection | projectTestConnection | scm.rs:187 | ✓ |

> 注：`POST /projects/scm-probe`、`POST /projects/scm-branches`（项目创建表单的测试连接/分支预填）后端已实现（scm.rs:119 / scm.rs:147），但 **handler 未挂**——后端先就绪，按「即删」纪律不挂 handler。见下「后端已实现但 handler 未挂」。

### 流水线定义 / 清单 / 统计（pipelines.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | /projects/:name/pipelines/:pipeline | pipelineDefinition | pipelines.rs:68 | ✓ |
| PUT | /projects/:name/pipelines/:pipeline | pipelineDefinitionSave | pipelines.rs:109 | ✓ |
| GET | /pipelines | pipelinesList | — | ✗（#105 契约先行：跨项目权威清单，后端无 GET /pipelines）|
| GET | /projects/:name/pipelines/:pipeline/stats | pipelineStats | — | ✗（#102 契约先行：流水线统计端点，后端无 stats）|

### 构建 / 产物（builds.rs / artifacts.rs / logs.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | …/builds | buildList | builds.rs:500 | ✓ |
| GET | …/builds/:number | buildDetail | builds.rs:542 | ✓ |
| POST | …/builds | trigger | builds.rs:349 | ✓ |
| POST | …/builds/:number/cancel | cancel | builds.rs:402 | ✓ |
| POST | …/builds/:number/rerun | rerun | builds.rs:437 | ✓ |
| DELETE | …/builds/:number | removeBuild | builds.rs:590 | ✓ |
| GET | …/builds/:number/artifacts | artifacts | artifacts.rs:248 | ✓ |
| GET | …/builds/:number/artifacts/:artifact | artifactDownload | artifacts.rs:291 | ✓ |

> 注：构建日志 SSE / 下载（`logs.rs` download + stream）后端已实现，前端经 `eventSource.ts` SSE 替身在 demo 走通——非 REST handler 面，不入本表。

### 构建机 / 升级（agents.rs / upgrade_packages.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | /agents | agentsList | agents.rs:499 | ✓ |
| GET | /agents/:name | agentDetail | agents.rs:524 | ✓ |
| PATCH | /agents/:name | agentPatch | agents.rs:553 | ✓ |
| POST | /agents/:name/workspace/list | agentWorkspaceList | agents.rs:799 | ✓ |
| POST | /agents/:name/workspace/clean | agentWorkspaceClean | agents.rs:853 | ✓ |
| POST | /agents/:name/cache/list | agentCacheList | agents.rs:887 | ✓ |
| POST | /agents/:name/cache/delete | agentCacheDelete | agents.rs:936 | ✓ |
| GET | /upgrade-packages | upgradePackagesList | upgrade_packages.rs:159 | ✓ |
| POST | /upgrade-packages | upgradePackageUpload | upgrade_packages.rs:67 | ✓ |
| DELETE | /upgrade-packages/:name | upgradePackageDelete | upgrade_packages.rs:180 | ✓ |
| POST | /agents/upgrade | agentsUpgradeAll | agents.rs:626 | ✓ |
| POST | /agents/:name/upgrade | agentsUpgradeOne | agents.rs:703 | ✓ |
| POST | /agents | agentCreate | agents.rs:312 | ✓ |

### 机密 / 审计 / 用户（secrets.rs / audit.rs / users.rs）

| 方法 | 路径 | handler | 后端 | 状态 |
|---|---|---|---|---|
| GET | /projects/:name/secrets | secretsList | secrets.rs:123 | ✓ |
| PUT | /projects/:name/secrets/:secret | secretPut | secrets.rs:69 | ✓ |
| DELETE | /projects/:name/secrets/:secret | secretDelete | secrets.rs:152 | ✓ |
| GET | /audit | auditList | audit.rs:87 | ✓ |
| GET | /users | usersList | users.rs:100 | ✓ |
| POST | /users | userCreate | users.rs:122 | ✓ |
| PATCH | /users/:name | userPatch | users.rs:167 | ✓ |
| PUT | /users/:name/password | userResetPassword | users.rs:219 | ✓ |

## 契约先行清单（后端待实现，6 端点 / 4 域）

这 6 个 handler 无后端对应——demo/vitest 走 mock 可用，`npm run dev`（proxy 真后端）会 404/穿透。后端照契约票实现后，dev 模式自动切真（demo/vitest 仍走 mock 直到 demo 退役）。

| 端点 | 契约票 | 说明 |
|---|---|---|
| GET /api/v1/pipelines | #105（P1）| 跨项目权威流水线清单（替代前端探测 main/release 凑数）；响应 `{ items: [{ project, pipeline, updated_at }], total }`，服务端 (project, pipeline) 字典序。|
| GET /api/v1/projects/:name/pipelines/:pipeline/stats | #102 | 流水线统计（窗口成功率/平均耗时/最近构建）；口径与构建列表同源聚合。|
| PATCH /api/v1/projects/:name | #108 | 编辑项目（scm_url 整段替换、svn 无分支校验）；后端有意缺口——项目编辑/删除的级联语义（流水线删除 vs 构建历史）未裁定。|
| GET /api/v1/user/pipeline-favorites | #104（W8）| 用户级收藏清单（按会话用户归属，latest_build 服务端 join 单请求成行）。|
| PUT /api/v1/user/pipeline-favorites/:project/:pipeline | #104（W8）| 收藏流水线。|
| DELETE /api/v1/user/pipeline-favorites/:project/:pipeline | #104（W8）| 取消收藏。|

> favorites 整域（3 端点）后端源码完全不存在——是当前最大的契约缺口。工作台「收藏的流水线」右栏（#104）+ 流水线页星标入口（#105）均依赖之。

## 后端已实现但 handler 未挂（「即删」纪律在 demo 下的可见代价）

| 端点 | 后端 | 前端消费 | demo 影响 |
|---|---|---|---|
| POST /api/v1/projects/scm-probe | scm.rs:119 | ProjectsView 项目创建表单「测试连接」（返回 head）| demo 模式无 mock 兜底 → MSW `onUnhandledRequest: 'bypass'` → vite proxy 无后端 → 请求失败，测试连接按钮报错（项目创建本身 POST /projects 有 handler，不受影响）。|
| POST /api/v1/projects/scm-branches | scm.rs:147 | ProjectsView 项目创建表单「预填默认分支」| 同上。|

这两个端点后端先就绪，按 ADR-0024「后端就绪即删」纪律**不挂 handler**。smoke 经 `page.route` 内联 mock 守该动作的渲染面（断言 head + 预填分支文案），但 demo 体验在该动作上断档——这是「即删」纪律在无后端 demo 下的可见代价，记录在案。

## 后端已实现但 v1 前端未消费（无对应 handler，非对账面）

下列后端端点 v1 前端无 UI / 无 mock（不入 53 handler 对账），记录以备后端实现状态全景：

- `POST /auth/register`（auth.rs:159，注册开关默认关）、`POST /auth/password`（auth.rs:371，改密）——v1 无自助注册/改密 UI。
- `POST /agent/register`（agents.rs:403，注册码换 agent token）、`POST /agent/artifacts/*`、`GET /agent/upgrade-packages/:name`（agent-token 面）——Agent 侧通道，前端不经 REST 消费。
- `GET/POST /projects/:name/pipelines/:pipeline/triggers`、`GET/PATCH …/triggers/:kind`（triggers.rs）——v1 无触发器管理 UI。
- `GET/PUT /config/smtp`（smtp_config.rs）——v1 无 SMTP 配置 UI。
- `GET /healthz`、`GET /metrics`（infra 面）——运维面，前端不经 SPA 消费。

## 后端有意缺口（v1 前端未消费，后端阶段裁定）

- `PATCH/PUT /api/v1/projects/:name`——见契约先行清单（#108）。
- `DELETE /api/v1/projects/:name`——项目删除级联语义（流水线删除 vs 构建历史）未裁定，后端 deferred。v1 前端无删除项目 UI。
- `DELETE /api/v1/projects/:name/pipelines/:pipeline/triggers/:kind`——触发器删除同上级联 concern，后端 deferred。v1 前端无触发器 UI。

## 收敛建议（post-v1）

「mock 层收敛为零」的触发条件是 demo 退役或改跑真后端 + seed。届时按本表 ✓ 列逐 handler 删除（dev 模式已切真，删 handler 不影响 dev；demo/vitest 在退役前仍需全量）。契约先行 6 端点先由后端照契约票实现，再纳入收敛。

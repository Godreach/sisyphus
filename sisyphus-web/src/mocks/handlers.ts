// MSW 契约 handlers（ADR-0024，票 #101）：严格按前端 api 层类型/ADR-0005 REST
// 契约实现「登录 → 概览 → 项目/流水线 → 构建列表/详情/产物 → Agent 清单」
// 核心链路 + 触发/取消/重跑。同一套 handlers 供 dev worker（authEnforced=true，
// 会话 cookie 生效）与 vitest node 模式（authEnforced=false，直连不校验）。
//
// - 后端每就绪一个端点即删除对应 handler（ADR-0024 handler 生命周期）。
// - 动态构建走 engine.ts；fixture 构建走 db.ts；两者在列表/详情处合并
//   （同号动态优先——from_failed 重跑会以动态态接管同号构建）。
// - 错误态 fixture：`error-demo` 项目全部端点 500；概览支持 `?_mock_error=1`。

import { http, HttpResponse, delay } from 'msw'

import type {
  AgentResponse,
  BuildAcceptedResponse,
  BuildListResponse,
  BuildStatusDto,
  BuildSummaryResponse,
  CreatedAgentResponse,
  CreateProjectRequest,
  MemberAssignment,
  ProjectResponse,
  UpdateProjectRequest,
} from '@/api/types'
import type { MeResponse } from '@/api/http'
import type { Pipeline as ModelPipelineDef } from '@/model/pipeline'
import { validatePipeline } from '@/model/validate'
import * as db from './db'
import {
  allDynamicSummaries,
  cancelBuild,
  dynamicArtifacts,
  dynamicBuild,
  dynamicDetail,
  dynamicSummaries,
  removeBuild as removeDynamicBuild,
  rerunFromFailed,
  triggerBuild,
} from './engine'

/** mock 会话 cookie 名（浏览器态登录/登出/会话恢复）。 */
export const SESSION_COOKIE = 'sisyphus_mock_session'

export interface MockHandlerOptions {
  /** 浏览器 worker：校验 mock 会话 cookie（未登录 401）；vitest node：关闭。 */
  authEnforced: boolean
}

function jsonError(status: number, code: string, message: string) {
  return HttpResponse.json({ code, message, detail: null }, { status })
}

/** 校验失败 422（server projects.rs validate 同形：detail.errors 错误清单）。 */
function validationError(errors: { path: string; message: string }[]) {
  // 契约序列化只取 path/message（调用方传 ValidationError 等超集形态时剥掉
  // 规则码等前端内部字段——响应形态与 server ValidationIssue 严格同形）。
  const items = errors.map(({ path, message }) => ({ path, message }))
  return HttpResponse.json(
    { code: 'VALIDATION_FAILED', message: '项目输入校验失败', detail: { errors: items } },
    { status: 422 },
  )
}

/** 项目域授权守卫（server policy.rs 判定序列同形，票 B2b-T5/ADR-0014）：
 *  无成员角色（或项目不存在）→ 404 同形（不可借 403/404 之辨探测存在性）；
 *  有角色但档位不足 admin → 403；admin → null 放行。 */
function projectAdminGuard(user: string, name: string): ReturnType<typeof jsonError> | null {
  const role = db.projectRoleOf(user, name)
  if (role == null) return jsonError(404, 'NOT_FOUND', '项目不存在')
  if (role !== 'admin') return jsonError(403, 'FORBIDDEN', '项目权限不足')
  return null
}

function sessionUser(request: Request): string | null {
  // 会话读取三处来源按序：①浏览器态 document.cookie（SW 的 fetch 事件请求
  //  不带 Cookie 头，cookie 由网络栈在 SW 之后附加）；②请求 Cookie 头（node
  //  直连态）；③mock-only 测试缝 `x-sisyphus-mock-user`（undici 按 Fetch 规范
  //  丢弃 Cookie 头、且 jsdom 的 document.cookie 恒为空——角色分流测试显式
  //  声明；浏览器 worker 永不携带此头）。jsdom 环境下两处 cookie 源皆空，
  //  顺序兼容三种宿主。
  const sources: string[] = []
  if (typeof document !== 'undefined') sources.push(document.cookie)
  sources.push(request.headers.get('cookie') ?? '')
  for (const raw of sources) {
    const match = raw
      .split(';')
      .map((c) => c.trim())
      .find((c) => c.startsWith(`${SESSION_COOKIE}=`))
    if (match) return decodeURIComponent(match.slice(SESSION_COOKIE.length + 1))
  }
  return request.headers.get('x-sisyphus-mock-user')
}

const sessions = new Set<string>()

/** 会话守卫（authEnforced 时未登录一律 401，与真后端 401 形态一致）。 */
function guard(options: MockHandlerOptions, request: Request) {
  if (!options.authEnforced) return null
  if (sessionUser(request) != null) return null
  return jsonError(401, 'UNAUTHORIZED', '未登录或会话已过期')
}

/** 错误态 fixture：error-demo 项目全端点 500（演示整页报错/重试）。 */
function isErrorFixture(name: string): boolean {
  return name === 'error-demo'
}

/** fixture + 动态构建合并（同号动态优先——from_failed 重跑以动态态接管）。
 *  构建列表与统计端点共用的单一合并点。 */
function mergedSummaries(project: string, pipeline: string): BuildSummaryResponse[] {
  const byNumber = new Map<number, BuildSummaryResponse>()
  for (const s of [...db.buildSummaries(project, pipeline), ...dynamicSummaries(project, pipeline)]) {
    byNumber.set(s.number, s)
  }
  return [...byNumber.values()]
}

export function createHandlers(options: MockHandlerOptions) {
  const h = {
    // ----- 认证（后端 api/auth.rs，ADR-0014）-----
    login: http.post('/api/v1/auth/login', async ({ request }) => {
      await delay(300)
      const body = (await request.json()) as {
        username?: string
        password?: string
        remember_me?: boolean
      }
      // 演示便利：任意非空账号密码均可登录（仍是真实 POST 往返；空凭据
      // 走 401 失败路径）。固定演示账号见 db.USERS（admin 为管理员）。
      const username = body.username?.trim() ?? ''
      if (username === '' || !body.password) {
        return jsonError(401, 'INVALID_CREDENTIALS', '用户名或密码错误')
      }
      const user = db.USERS.find((u) => u.username === username) ?? {
        id: 0,
        username,
        password: '',
        is_admin: username === 'admin',
      }
      sessions.add(user.username)
      return HttpResponse.json(
        { username: user.username, is_admin: user.is_admin } satisfies MeResponse,
        {
          status: 200,
          // 注意：mock 会话 cookie 不带 HttpOnly——MSW worker 经 document.cookie
          // 落 cookie，HttpOnly 属性会被浏览器丢弃导致会话立不住。
          // remember_me（契约先行，票 #114）：勾选带 30 天 Max-Age（保持登录），
          // 否则会话级 cookie（关浏览器即失效）。
          headers: {
            'Set-Cookie': `${SESSION_COOKIE}=${encodeURIComponent(user.username)}; Path=/; SameSite=Lax${
              body.remember_me ? '; Max-Age=2592000' : ''
            }`,
          },
        },
      )
    }),

    logout: http.post('/api/v1/auth/logout', ({ request }) => {
      sessions.delete(sessionUser(request) ?? '')
      return new HttpResponse(null, {
        status: 204,
        headers: { 'Set-Cookie': `${SESSION_COOKIE}=; Path=/; SameSite=Lax; Max-Age=0` },
      })
    }),

    me: http.get('/api/v1/auth/me', ({ request }) => {
      const user = sessionUser(request)
      if (options.authEnforced && user == null) {
        return jsonError(401, 'UNAUTHORIZED', '未登录或会话已过期')
      }
      const known = db.USERS.find((u) => u.username === (user ?? 'admin'))
      return HttpResponse.json({
        username: known?.username ?? 'admin',
        is_admin: known?.is_admin ?? true,
      } satisfies MeResponse)
    }),

    // 空库判定（auth store isSetupNeeded 探测）：探针用非法输入（空用户名），
    // mock 库非空 → 404 = 引导已完成（守卫放 guest 去 /login）。
    // 合法提交（SetupView 首装建号，直访 /setup 演示）：201 模拟空库建号成功。
    setup: http.post('/api/v1/auth/setup', async ({ request }) => {
      await delay(250)
      const body = (await request.json()) as { username?: string; password?: string }
      const username = body.username?.trim() ?? ''
      if (username === '' || !body.password) {
        return jsonError(404, 'NOT_FOUND', '初始化已完成')
      }
      return HttpResponse.json(
        { username, is_admin: true } satisfies MeResponse,
        { status: 201 },
      )
    }),

    // ----- 首装引导 / 页面动作：建 Agent / 建项目（fixture 增量追加）-----
    agentCreate: http.post('/api/v1/agents', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(250)
      const body = (await request.json()) as {
        name?: string
        custom_labels?: string[]
        max_concurrency?: number
      }
      const name = body.name?.trim() || `build-${db.AGENTS.length + 1}`
      const agent: AgentResponse = {
        name,
        online: false,
        disabled: false,
        system_labels: ['linux', 'docker'],
        custom_labels: body.custom_labels ?? [],
        max_concurrency: body.max_concurrency ?? 4,
        active_jobs: 0,
        last_seen_at: null,
        disk_usage: null,
        agent_version: null,
        version_compatible: true,
        draining: false,
        upgrade_phase: null,
        upgrade_error: null,
        // 未上线无心跳：利用率无值（「—」路径，契约票 #102）。
        cpu_usage: null,
        memory_usage: null,
        created_at: Date.now(),
        updated_at: Date.now(),
      }
      db.AGENTS.push(agent)
      // token 与注册码明文仅此一次返回（ADR-0010）。
      return HttpResponse.json(
        {
          token: `sis_${Array.from({ length: 43 }, () => 'abcdefghijklmnopqrstuvwxyz234567'[Math.floor(Math.random() * 32)]).join('')}`,
          register_code: `sisyphus register ${name} --code ${Math.random().toString(36).slice(2, 10).toUpperCase()}`,
          agent,
        } satisfies CreatedAgentResponse,
        { status: 201 },
      )
    }),

    projectCreate: http.post('/api/v1/projects', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(250)
      const body = (await request.json()) as CreateProjectRequest
      const project: ProjectResponse = {
        id: db.PROJECTS.length + 1,
        name: body.name,
        scm_type: body.scm_type,
        scm_url: body.scm_url,
        default_branch: body.default_branch ?? null,
        created_at: Date.now(),
        updated_at: Date.now(),
      }
      db.PROJECTS.push(project)
      return HttpResponse.json(project, { status: 201 })
    }),

    // ----- 概览（后端 api/overview.rs，ADR-0019）-----
    overview: http.get('/api/v1/overview', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(200)
      if (new URL(request.url).searchParams.get('_mock_error') === '1') {
        return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
      }
      // 契约票 #104（W4）：快照合并动态构建（最近构建可见排队/运行中动态态，
      // 队列深度同口径）——触发成功后前端刷新快照即见新构建（W2 闭环）。
      return HttpResponse.json(db.overviewSnapshot(allDynamicSummaries()))
    }),

    // ----- 收藏流水线（契约票 #104，W8 裁定；用户级，按会话用户归属）-----
    favoritesList: http.get('/api/v1/user/pipeline-favorites', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(150)
      const user = sessionUser(request) ?? 'admin'
      return HttpResponse.json(
        db.favoritesOf(user).map((f) =>
          db.favoriteResponse(
            f,
            db.latestBuildOf(mergedSummaries(f.project, f.pipeline)),
          ),
        ),
      )
    }),

    favoriteAdd: http.put(
      '/api/v1/user/pipeline-favorites/:project/:pipeline',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        await delay(150)
        const user = sessionUser(request) ?? 'admin'
        const ok = db.addFavorite(user, String(params.project), String(params.pipeline))
        if (!ok) return jsonError(404, 'NOT_FOUND', '流水线不存在')
        return new HttpResponse(null, { status: 204 })
      },
    ),

    favoriteRemove: http.delete(
      '/api/v1/user/pipeline-favorites/:project/:pipeline',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        await delay(150)
        const user = sessionUser(request) ?? 'admin'
        db.removeFavorite(user, String(params.project), String(params.pipeline))
        return new HttpResponse(null, { status: 204 })
      },
    ),

    // ----- 项目 / 流水线定义 -----
    projectsList: http.get('/api/v1/projects', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(200)
      // 可见性过滤（server projects.rs list 同形）：全局 admin 全量、普通用户
      // 仅显式成员项目（node 模式缺省用户 admin → 全量，与既有测试一致）。
      const user = sessionUser(request) ?? 'admin'
      const visible = db.PROJECTS.filter((p) => db.projectRoleOf(user, p.name) != null)
      return HttpResponse.json(visible)
    }),

    // ----- 项目详情 / 编辑 / 成员 / 用户目录 / SCM 凭据（票 #108；成员+凭据
    //       端点属后端 members.rs/scm.rs 既有 REST 面，编辑项目为契约先行——
    //       本票即契约冻结点，后端阶段照单实现）-----
    projectGet: http.get('/api/v1/projects/:name', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      const name = String(params.name)
      await delay(150)
      if (isErrorFixture(name)) {
        return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
      }
      const user = sessionUser(request) ?? 'admin'
      const role = db.projectRoleOf(user, name)
      const p = db.PROJECTS.find((x) => x.name === name)
      // 无角色与不存在同形 404（B2b-T5：不暴露存在性）。
      if (p == null || role == null) return jsonError(404, 'NOT_FOUND', '项目不存在')
      return HttpResponse.json(p)
    }),

    // 编辑项目（契约先行）：PATCH 语义 + create 同规则校验（svn 无分支；
    //  scm_url 提交即整段替换、非空）。项目 admin 档守卫见 projectAdminGuard。
    projectPatch: http.patch('/api/v1/projects/:name', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      const name = String(params.name)
      await delay(250)
      if (isErrorFixture(name)) {
        return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
      }
      const user = sessionUser(request) ?? 'admin'
      const deniedAdmin = projectAdminGuard(user, name)
      if (deniedAdmin != null) return deniedAdmin
      const body = (await request.json()) as UpdateProjectRequest
      const current = db.PROJECTS.find((x) => x.name === name)
      const issues: { path: string; message: string }[] = []
      if (body.scm_url != null && body.scm_url.trim() === '') {
        issues.push({ path: 'scm_url', message: '仓库 URL 不能为空' })
      }
      if (current?.scm_type === 'svn' && body.default_branch != null) {
        issues.push({ path: 'default_branch', message: 'svn 项目无分支概念，不支持默认分支' })
      }
      if (issues.length > 0) return validationError(issues)
      const updated = db.updateProject(name, {
        scm_url: body.scm_url?.trim(),
        default_branch: body.default_branch,
      })
      return HttpResponse.json(updated)
    }),

    projectMembersList: http.get('/api/v1/projects/:name/members', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      const name = String(params.name)
      await delay(150)
      if (isErrorFixture(name)) {
        return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
      }
      const user = sessionUser(request) ?? 'admin'
      const deniedAdmin = projectAdminGuard(user, name)
      if (deniedAdmin != null) return deniedAdmin
      return HttpResponse.json(db.membersOf(name))
    }),

    projectMembersReplace: http.put('/api/v1/projects/:name/members', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      const name = String(params.name)
      await delay(250)
      if (isErrorFixture(name)) {
        return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
      }
      const user = sessionUser(request) ?? 'admin'
      const deniedAdmin = projectAdminGuard(user, name)
      if (deniedAdmin != null) return deniedAdmin
      const body = (await request.json()) as MemberAssignment[]
      // server members.rs:156 同义——整组替换含不存在用户即 400。
      const unknown = db.unknownMemberOf(body)
      if (unknown != null) {
        return jsonError(400, 'BAD_REQUEST', `用户不存在：${unknown}`)
      }
      return HttpResponse.json(db.replaceMembersOf(name, body))
    }),

    usersDirectory: http.get('/api/v1/users/directory', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(120)
      const user = sessionUser(request) ?? 'admin'
      // server users.rs directory：全局 admin 或任一项目 admin，其余 403。
      if (!db.isAnyProjectAdmin(user)) {
        return jsonError(403, 'FORBIDDEN', '用户目录仅项目或全局管理员可读（成员分配用）')
      }
      return HttpResponse.json(db.USERS.map((u) => ({ id: u.id, username: u.username })))
    }),

    projectScmCredential: http.put(
      '/api/v1/projects/:name/scm-credential',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        await delay(200)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const user = sessionUser(request) ?? 'admin'
        const deniedAdmin = projectAdminGuard(user, name)
        if (deniedAdmin != null) return deniedAdmin
        // 204 空体（凭据任何端点不回显，ADR-0015/0016）。
        return new HttpResponse(null, { status: 204 })
      },
    ),

    projectTestConnection: http.post(
      '/api/v1/projects/:name/test-connection',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        await delay(300)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const user = sessionUser(request) ?? 'admin'
        const deniedAdmin = projectAdminGuard(user, name)
        if (deniedAdmin != null) return deniedAdmin
        // 确定性 head（按项目名散列）：页面徽章「连接成功，当前 head：…」可演示。
        const rng = db.mulberry32([...name].reduce((s, c) => s + c.charCodeAt(0), 0))
        const head = Array.from({ length: 12 }, () => '0123456789abcdef'[Math.floor(rng() * 16)]).join('')
        return HttpResponse.json({ head })
      },
    ),

    pipelineDefinition: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        await delay(120)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        // viewer 档（server policy.rs 判定序列同形）：无成员角色与项目不存在同形 404。
        const user = sessionUser(request) ?? 'admin'
        if (db.projectRoleOf(user, name) == null) {
          return jsonError(404, 'NOT_FOUND', '项目不存在')
        }
        const stored = db.getPipelineDefinition(name, pipeline)
        if (stored == null) return jsonError(404, 'NOT_FOUND', '流水线不存在')
        return HttpResponse.json({
          definition: stored.definition,
          revision: stored.revision,
          operator: stored.operator,
          updated_at: stored.updated_at,
        })
      },
    ),

    // ----- Pipeline 定义保存（票 #109 编辑器闭环）：admin 档守卫 + model 校验
    // 422 整组透传 + overlay 持久化（保存→重载 revision 递增演示闭环）-----
    pipelineDefinitionSave: http.put(
      '/api/v1/projects/:name/pipelines/:pipeline',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        await delay(200)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const user = sessionUser(request) ?? 'admin'
        // admin 档守卫（server pipelines.rs RequireAdmin 同形）：无角色 404 同形
        // （不泄露存在性）/ 档位不足 403。
        const deniedAdmin = projectAdminGuard(user, name)
        if (deniedAdmin != null) return deniedAdmin
        // 形态错也是校验失败（server parse_body 同形：path="$"），不落 model 校验。
        const body = (await request.json()) as ModelPipelineDef
        if (
          body == null ||
          !Array.isArray(body.parameters) ||
          !Array.isArray(body.env) ||
          !Array.isArray(body.stages)
        ) {
          return validationError([{ path: '$', message: '请求体形态不符 model Pipeline' }])
        }
        // model 校验失败 422 整组透传（与前端本地校验同一 TS 端口——单一事实源，
        // server InvalidDefinition 同义；ValidationError 的 code 字段经
        // validationError 剥离，响应形态与 server ValidationIssue 同形）。
        const errors = validatePipeline(body)
        if (errors.length > 0) {
          return validationError(errors)
        }
        const stored = db.savePipelineDefinition(name, pipeline, body, user)
        return HttpResponse.json({
          revision: stored.revision,
          operator: stored.operator,
          updated_at: stored.updated_at,
        })
      },
    ),

    // ----- 流水线清单（契约票 #105，P1 裁定：跨项目权威清单，替代前端探测）-----
    pipelinesList: http.get('/api/v1/pipelines', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(150)
      return HttpResponse.json(db.pipelineListItems())
    }),

    // ----- 流水线统计（契约票 #102：fixture + 动态合并聚合，口径同构建列表）-----
    pipelineStats: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline/stats',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        await delay(120)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        if (db.findPipeline(name, pipeline) == null) {
          return jsonError(404, 'NOT_FOUND', '流水线不存在')
        }
        const url = new URL(request.url)
        const rawWindow = url.searchParams.get('window')
        // 窗口钳制单点在 db.pipelineStatsFrom（缺省/非数值 → 缺省 20；越界 → 边界）。
        const window =
          rawWindow != null && rawWindow !== '' ? Number(rawWindow) : db.STATS_WINDOW_DEFAULT
        const all = mergedSummaries(name, pipeline)
        return HttpResponse.json(db.pipelineStatsFrom(all, all.length, window))
      },
    ),

    // ----- 构建（后端 api/builds.rs，ADR-0006/0008/0013）-----
    buildList: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline/builds',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        await delay(150)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        if (db.findPipeline(name, pipeline) == null) {
          return jsonError(404, 'NOT_FOUND', '流水线不存在')
        }
        const url = new URL(request.url)
        const statusParam = url.searchParams.get('status')
        const status = (statusParam === '' || statusParam == null ? null : statusParam) as
          | BuildStatusDto
          | null
        const page = Math.max(1, Number(url.searchParams.get('page') ?? '1'))
        const limit = Math.min(200, Math.max(1, Number(url.searchParams.get('limit') ?? '20')))

        const all = mergedSummaries(name, pipeline)
          .filter((s) => status == null || s.status === status)
          .sort((a, b) => b.number - a.number)
        const items = all.slice((page - 1) * limit, page * limit)
        return HttpResponse.json({
          items,
          total: all.length,
          page,
          limit,
        } satisfies BuildListResponse)
      },
    ),

    buildDetail: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        const number = Number(params.number)
        await delay(120)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const dyn = dynamicDetail(name, pipeline, number)
        if (dyn != null) return HttpResponse.json(dyn)
        const detail = db.buildDetailOf(name, pipeline, number)
        if (detail == null) return jsonError(404, 'NOT_FOUND', '构建不存在')
        return HttpResponse.json(detail)
      },
    ),

    trigger: http.post(
      '/api/v1/projects/:name/pipelines/:pipeline/builds',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        await delay(250)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const body = (await request.json().catch(() => ({}))) as {
          params?: Record<string, string>
        }
        const accepted = triggerBuild(name, pipeline, {
          params: body.params,
          triggerBy: sessionUser(request) ?? 'admin',
        })
        if (accepted == null) return jsonError(404, 'NOT_FOUND', '流水线不存在')
        return HttpResponse.json(accepted, { status: 202 })
      },
    ),

    cancel: http.post(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number/cancel',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        const number = Number(params.number)
        await delay(200)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        // 动态构建：排队/运行置 cancelled（活跃日志流推 job_end 终态关流）；
        // fixture 构建：queued/running 迁移 cancelled 终态，终态幂等 202。
        const dyn = cancelBuild(name, pipeline, number)
        if (dyn != null) return HttpResponse.json(dyn, { status: 202 })
        const summary = db.cancelFixtureBuild(name, pipeline, number)
        if (summary == null) return jsonError(404, 'NOT_FOUND', '构建不存在')
        return HttpResponse.json(
          {
            number: summary.number,
            build_id: summary.number,
            attempt: summary.attempt,
            status: summary.status,
          } satisfies BuildAcceptedResponse,
          { status: 202 },
        )
      },
    ),

    rerun: http.post(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number/rerun',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        const number = Number(params.number)
        await delay(250)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const body = (await request.json().catch(() => ({}))) as { mode?: string }
        if (body.mode === 'from_failed') {
          const current = dynamicBuild(name, pipeline, number) ?? db.buildSummaryAt(name, pipeline, number)
          const status = current?.status ?? null
          if (status !== 'failed') {
            return jsonError(409, 'CONFLICT', '仅失败终态可从失败任务重跑')
          }
          const accepted = rerunFromFailed(name, pipeline, number, sessionUser(request) ?? 'admin')
          if (accepted == null) return jsonError(404, 'NOT_FOUND', '构建不存在')
          return HttpResponse.json(accepted, { status: 202 })
        }
        // from_scratch：从头重跑 = 新号触发。
        const accepted = triggerBuild(name, pipeline, { triggerBy: sessionUser(request) ?? 'admin' })
        if (accepted == null) return jsonError(404, 'NOT_FOUND', '流水线不存在')
        return HttpResponse.json(accepted, { status: 202 })
      },
    ),

    removeBuild: http.delete(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        const number = Number(params.number)
        await delay(200)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const live = dynamicDetail(name, pipeline, number)
        if (live != null && (live.status === 'queued' || live.status === 'running')) {
          return jsonError(409, 'CONFLICT', '运行中/排队的构建不可删除')
        }
        const removed =
          db.deleteBuildRecord(name, pipeline, number) ||
          dynamicBuild(name, pipeline, number) != null
        if (!removed) return jsonError(404, 'NOT_FOUND', '构建不存在')
        removeDynamicBuild(name, pipeline, number)
        // 成功 204：该构建的日志与产物全删、列表/详情/产物随之不可见
        // （fixture 记录保留在源数据，mock 可见面摘除——ADR-0013 语义）。
        return new HttpResponse(null, { status: 204 })
      },
    ),

    artifacts: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number/artifacts',
      async ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        const name = String(params.name)
        const pipeline = String(params.pipeline)
        const number = Number(params.number)
        await delay(120)
        if (isErrorFixture(name)) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        const dyn = dynamicArtifacts(name, pipeline, number)
        if (dyn != null) return HttpResponse.json({ items: dyn })
        if (db.buildSummaryAt(name, pipeline, number) == null) {
          return jsonError(404, 'NOT_FOUND', '构建不存在')
        }
        return HttpResponse.json({ items: db.artifactsOf(name, pipeline, number) })
      },
    ),

    artifactDownload: http.get(
      '/api/v1/projects/:name/pipelines/:pipeline/builds/:number/artifacts/:artifact',
      ({ request, params }) => {
        const denied = guard(options, request)
        if (denied != null) return denied
        if (isErrorFixture(String(params.name))) {
          return jsonError(500, 'INTERNAL', '服务内部错误（mock 错误态演示）')
        }
        return new HttpResponse(`mock artifact: ${String(params.artifact)}\n`, {
          status: 200,
          headers: {
            'Content-Type': 'application/octet-stream',
            'Content-Disposition': `attachment; filename="${String(params.artifact)}"`,
          },
        })
      },
    ),

    // ----- Agent 清单（后端 api/agents.rs；构建机页消费）-----
    agentsList: http.get('/api/v1/agents', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(150)
      return HttpResponse.json(db.AGENTS)
    }),

    // ----- 构建机详情 / 编辑停用 / 工作区 / 缓存（票 #106，M1/M4 契约缺口：
    //       后端阶段照单实现；形态按 api/agents.rs 既有契约）-----
    agentDetail: http.get('/api/v1/agents/:name', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(150)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      return HttpResponse.json(agent)
    }),

    agentPatch: http.patch('/api/v1/agents/:name', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(250)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      const body = (await request.json()) as {
        disabled?: boolean
        max_concurrency?: number
        custom_labels?: string[]
      }
      if (body.disabled != null) {
        agent.disabled = body.disabled
        // 停用即踢线（ADR-0008：停用 Agent 不再参与调度，连接关闭）。
        // 重新启用不就地恢复 online——真实语义是等 Agent 进程重连心跳
        // （mock 会话内无心跳源，启用后保持离线是正确形态，非缺陷）。
        if (body.disabled) {
          agent.online = false
          agent.active_jobs = 0
        }
      }
      if (body.max_concurrency != null) agent.max_concurrency = body.max_concurrency
      if (body.custom_labels != null) agent.custom_labels = body.custom_labels
      agent.updated_at = Date.now()
      return HttpResponse.json(agent)
    }),

    // 工作区 / 缓存走通道往返（ADR-0011/0012）：离线 → 409，fire-and-forget
    // 清理/删除 → 202 空体。fixture 只给在线机器造条目（离线机器列表不可达）。
    agentWorkspaceList: http.post('/api/v1/agents/:name/workspace/list', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(300)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      if (!agent.online) {
        return jsonError(409, 'AGENT_OFFLINE', '构建机离线，无法经通道查询工作区')
      }
      return HttpResponse.json({ entries: db.workspaceEntriesOf(agent.name) })
    }),

    agentWorkspaceClean: http.post('/api/v1/agents/:name/workspace/clean', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(200)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      if (!agent.online) {
        return jsonError(409, 'AGENT_OFFLINE', '构建机离线，无法经通道下发清理指令')
      }
      return new HttpResponse(null, { status: 202 })
    }),

    agentCacheList: http.post('/api/v1/agents/:name/cache/list', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(300)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      if (!agent.online) {
        return jsonError(409, 'AGENT_OFFLINE', '构建机离线，无法经通道查询缓存')
      }
      return HttpResponse.json({ entries: db.cacheEntriesOf(agent.name) })
    }),

    agentCacheDelete: http.post('/api/v1/agents/:name/cache/delete', async ({ request, params }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(200)
      const agent = db.AGENTS.find((a) => a.name === String(params.name))
      if (agent == null) {
        return jsonError(404, 'AGENT_NOT_FOUND', `构建机 ${String(params.name)} 不存在`)
      }
      if (!agent.online) {
        return jsonError(409, 'AGENT_OFFLINE', '构建机离线，无法经通道下发删除指令')
      }
      return new HttpResponse(null, { status: 202 })
    }),
  }

  return Object.values(h)
}

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
  ProjectResponse,
} from '@/api/types'
import type { MeResponse } from '@/api/http'
import * as db from './db'
import {
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

function sessionUser(request: Request): string | null {
  // 浏览器态从 document.cookie 读会话：SW 的 fetch 事件请求不带 Cookie 头
  // （cookie 由网络栈在 SW 之后的网络层附加）；node 态回落请求头。
  const raw =
    typeof document !== 'undefined'
      ? document.cookie
      : (request.headers.get('cookie') ?? '')
  const match = raw
    .split(';')
    .map((c) => c.trim())
    .find((c) => c.startsWith(`${SESSION_COOKIE}=`))
  return match ? decodeURIComponent(match.slice(SESSION_COOKIE.length + 1)) : null
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
      return HttpResponse.json(db.overviewSnapshot())
    }),

    // ----- 项目 / 流水线定义 -----
    projectsList: http.get('/api/v1/projects', async ({ request }) => {
      const denied = guard(options, request)
      if (denied != null) return denied
      await delay(200)
      return HttpResponse.json(db.PROJECTS)
    }),

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
        const def = db.findPipeline(name, pipeline)
        if (def == null) return jsonError(404, 'NOT_FOUND', '流水线不存在')
        return HttpResponse.json({
          definition: {
            name: def.name,
            parameters: def.parameters,
            stages: def.stages,
          },
          revision: 3,
          operator: 'admin',
          updated_at: Date.now() - 86400e3,
        })
      },
    ),

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
  }

  return Object.values(h)
}

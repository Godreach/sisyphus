// 流水线页行为测试（票 #105 定稿：P1 清单契约 / P2 chips 口径 / P3 进度 /
// P4 脚注 / P6 默认卡片 / W8 收藏入口）。数据驱动：MSW node 模式（ADR-0024
// 单一缝）——组件经真实 http client 打 src/mocks handlers；需要确定性场景的
// 用例以 server.use 覆盖清单/统计/详情端点。只测外部行为（用户可见状态、
// DOM 事件、网络请求形态断言）。视图在 onMounted 即发请求：mount 须在
// handler 覆盖之后。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import PipelinesView from '@/views/PipelinesView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

/** 统计响应（契约票 #102 形态；latest = 最近一条构建，null = 从未运行）。 */
function statsBody(
  latest: { number: number; status: string } | null,
  opts: { total?: number; rate?: number | null; avg?: number | null } = {},
): Record<string, unknown> {
  const live = latest?.status === 'running' || latest?.status === 'queued'
  const queued = latest?.status === 'queued'
  return {
    window: 20,
    total_builds: opts.total ?? 5,
    terminal_count: 4,
    succeeded_count: 3,
    success_rate: opts.rate !== undefined ? opts.rate : 63.6,
    avg_duration_ms: opts.avg !== undefined ? opts.avg : 512_340,
    latest_build:
      latest == null
        ? null
        : {
            number: latest.number,
            status: latest.status,
            trigger: 'manual',
            started_at: queued ? null : 1_700_000_000_000,
            finished_at: live ? null : 1_700_000_060_000,
          },
  }
}

/** 覆盖流水线清单端点（P1 契约形态）。 */
function mockList(rows: { project: string; pipeline: string }[]): void {
  server.use(
    http.get('/api/v1/pipelines', () =>
      HttpResponse.json({
        items: rows.map((r) => ({ ...r, updated_at: 1_700_000_000_000 })),
        total: rows.length,
      }),
    ),
  )
}

/** 覆盖统计端点（key = "project/pipeline" → 响应体；未命中 404）。 */
function mockStats(map: Record<string, Record<string, unknown>>): void {
  server.use(
    http.get('/api/v1/projects/:project/pipelines/:pipeline/stats', ({ params }) => {
      const key = `${String(params.project)}/${String(params.pipeline)}`
      const body = map[key]
      return body != null
        ? HttpResponse.json(body)
        : HttpResponse.json({ code: 'NOT_FOUND', message: '流水线不存在', detail: null }, { status: 404 })
    }),
  )
}

/** 一条任务视图（JobViewDto 全字段）。 */
function job(name: string, status: string): Record<string, unknown> {
  return {
    name,
    status,
    attempt: 1,
    started_at: status === 'queued' ? null : 1_700_000_000_000,
    finished_at: status === 'succeeded' ? 1_700_000_030_000 : null,
    exit_code: status === 'succeeded' ? 0 : null,
    allow_failure: false,
    detail: null,
    agent_id: status === 'queued' ? null : 1,
  }
}

/** 覆盖构建详情端点（P3 进度数据源：1 成功 + 1 运行 + 1 排队 → 33%）。 */
function mockRunningDetail(project: string, pipeline: string, number: number): void {
  server.use(
    http.get(`/api/v1/projects/${project}/pipelines/${pipeline}/builds/${number}`, () =>
      HttpResponse.json({
        number,
        pipeline_name: pipeline,
        status: 'running',
        trigger: 'manual',
        trigger_by: 'admin',
        attempt: 1,
        started_at: 1_700_000_000_000,
        finished_at: null,
        cancelled_at: null,
        elapsed_ms: 5_000,
        stages: [
          { index: 0, name: 'build', jobs: [job('compile', 'succeeded'), job('unit-test', 'running')] },
          { index: 1, name: 'check', jobs: [job('lint', 'queued')] },
        ],
      }),
    ),
  )
}

/** 包装组件：NMessageProvider + PipelinesView（useMessage 注入可用）。 */
const Host = defineComponent({
  name: 'PipelinesHost',
  setup() {
    return () => h(NMessageProvider, () => h(PipelinesView))
  },
})

describe('PipelinesView 流水线页（#105 定稿）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** 经 MSW 观测到的请求（method + path，网络请求形态断言面）。 */
  let requests: string[]

  function mountView(): VueWrapper {
    wrapper = mount(Host, {
      global: { plugins: [pinia, router, i18n] },
    })
    return wrapper
  }

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/pipelines', name: 'pipelines', component: { template: '<div />' } },
        {
          path: '/projects/:name/pipelines/:pipeline/builds',
          name: 'build-list',
          component: { template: '<div />' },
        },
      ],
    })
    await router.push('/pipelines')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      requests.push(`${request.method} ${new URL(request.url).pathname}`)
    })
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
    server.resetHandlers()
    server.events.removeAllListeners()
  })

  afterAll(() => {
    server.close()
  })

  it('默认卡片视图（P6）：渲染 mock 清单全量 23 条；脚注「共 n 次构建」（P4）；未运行行显示「—」', async () => {
    const w = mountView()
    // 23 张卡各自还要打 stats 端点；全量并发负载下 1s 默认超时偶发不够（抖动）。
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(23), { timeout: 5_000 })
    // 默认视图 = 卡片（P6 裁定），卡片视图激活态在「卡片」钮上。
    expect(w.find('[data-testid="view-cards-btn"]').classes()).toContain('active')

    // P4：脚注 = 该流水线构建总数口径（main fixture 13 条），不再是「条流水线」。
    const feet = w.findAll('.p-card .trigger-tag').map((n) => n.text())
    expect(feet.some((txt) => txt === '共 13 次构建')).toBe(true)
    expect(feet.every((txt) => /^共 \d+ 次构建$/.test(txt))).toBe(true)

    // 契约「—」路径：empty-repo 流水线零构建 → 未运行 + 成功率/平均耗时「—」。
    const emptyCard = w.findAll('.p-card').find((c) => c.text().includes('empty-repo'))
    expect(emptyCard).toBeDefined()
    expect(emptyCard!.text()).toContain('未运行')
    expect(emptyCard!.text()).toContain('—')

    // 请求形态：清单 1 次 + 每行统计（23 行）。
    await vi.waitFor(() => expect(requests.filter((r) => r === 'GET /api/v1/pipelines')).toHaveLength(1))
    expect(requests.filter((r) => r.endsWith('/stats'))).toHaveLength(23)
  })

  it('chips 计数严格对账（P2）：全部 = 进行中 + 成功 + 失败 + 超时/取消；筛选生效', async () => {
    mockList([
      { project: 'demo', pipeline: 'main' },
      { project: 'demo', pipeline: 'ci' },
      { project: 'demo', pipeline: 'nightly' },
      { project: 'demo', pipeline: 'cron' },
      { project: 'demo', pipeline: 'fresh' },
    ])
    mockStats({
      'demo/main': statsBody({ number: 13, status: 'running' }),
      'demo/ci': statsBody({ number: 12, status: 'succeeded' }),
      'demo/nightly': statsBody({ number: 4, status: 'failed' }),
      'demo/cron': statsBody({ number: 2, status: 'cancelled' }),
      'demo/fresh': statsBody(null),
    })

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(5))

    const countOf = (key: string): number =>
      Number(w.find(`[data-testid="chip-${key}"] .count`).text())
    // 「进行中」含排队/运行（P2 改名口径）；「超时/取消」单列、「未运行」
    // 收留 latest 空行——计数严格对账（P2「计数能对上」）。
    expect(countOf('all')).toBe(5)
    expect(countOf('active')).toBe(1)
    expect(countOf('success')).toBe(1)
    expect(countOf('failed')).toBe(1)
    expect(countOf('ended')).toBe(1)
    expect(countOf('never')).toBe(1)
    expect(countOf('all')).toBe(
      countOf('active') + countOf('success') + countOf('failed') + countOf('ended') + countOf('never'),
    )

    // 筛选生效：点「进行中」只剩 running 行。
    await w.find('[data-testid="chip-active"]').trigger('click')
    expect(w.findAll('.p-card')).toHaveLength(1)
    expect(w.findAll('.p-card')[0]!.text()).toContain('main')

    // 点「超时/取消」只剩 cancelled 行。
    await w.find('[data-testid="chip-ended"]').trigger('click')
    expect(w.findAll('.p-card')).toHaveLength(1)
    expect(w.findAll('.p-card')[0]!.text()).toContain('cron')

    // 点「未运行」只剩 latest 空行。
    await w.find('[data-testid="chip-never"]').trigger('click')
    expect(w.findAll('.p-card')).toHaveLength(1)
    expect(w.findAll('.p-card')[0]!.text()).toContain('fresh')
  })

  it('进度（P3）：运行中行走构建详情算任务进度；非运行行显示「—」；排队/失败/成功各归其位', async () => {
    mockList([
      { project: 'demo', pipeline: 'main' },
      { project: 'demo', pipeline: 'ci' },
    ])
    mockStats({
      'demo/main': statsBody({ number: 13, status: 'running' }),
      'demo/ci': statsBody({ number: 12, status: 'succeeded' }),
    })
    mockRunningDetail('demo', 'main', 13)

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(2))

    // 卡片视图：运行中的 main 有进度条（1/3 任务落定 → 33%）。
    const mainCard = w.findAll('.p-card').find((c) => c.text().includes('main'))
    expect(mainCard).toBeDefined()
    expect(mainCard!.find('.p-card-progress .usage-row').exists()).toBe(true)
    expect(mainCard!.find('.p-card-progress .pct').text()).toBe('33%')
    expect(mainCard!.find('.p-card-progress .fill').attributes('style')).toContain('width: 33%')

    // 列表视图：进度列运行中显示进度条，其余行显示「—」。
    await w.find('[data-testid="view-list-btn"]').trigger('click')
    const rows = w.findAll('.pipe-row')
    expect(rows).toHaveLength(2)
    const mainRow = rows.find((r) => r.text().includes('main'))
    expect(mainRow!.find('.pc-progress .usage-row .fill').exists()).toBe(true)
    expect(mainRow!.find('.pc-progress .pct').text()).toBe('33%')
    const other = rows.find((r) => !r.text().includes('main'))
    expect(other!.find('.pc-progress .pct-none').text()).toBe('—')
  })

  it('成功率/平均耗时按契约形态（#102）：取不到显示「—」', async () => {
    mockList([{ project: 'demo', pipeline: 'main' }])
    mockStats({ 'demo/main': statsBody({ number: 13, status: 'queued' }, { rate: null, avg: null }) })

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(1))
    const card = w.findAll('.p-card')[0]!
    const stats = card.findAll('.p-stat').map((n) => n.text())
    expect(stats.some((s) => s.includes('成功率') && s.includes('—'))).toBe(true)
    expect(stats.some((s) => s.includes('平均耗时') && s.includes('—'))).toBe(true)
  })

  it('视图切换（P6）：列表 ⇄ 卡片；行内动作三态映射（运行/终止/重试）走真实端点', async () => {
    mockList([
      { project: 'demo', pipeline: 'ci' },
      { project: 'demo', pipeline: 'main' },
      { project: 'demo', pipeline: 'nightly' },
    ])
    mockStats({
      'demo/ci': statsBody({ number: 12, status: 'succeeded' }),
      'demo/main': statsBody({ number: 13, status: 'running' }),
      'demo/nightly': statsBody({ number: 4, status: 'failed' }),
    })
    // 动作端点覆盖（demo 项目不在 fixture 里，默认 handler 会 404）：
    // 受理响应走契约 202 形态。
    server.use(
      http.post('/api/v1/projects/demo/pipelines/ci/builds', () =>
        HttpResponse.json({ number: 13, build_id: 1, attempt: 1, status: 'queued' }, { status: 202 }),
      ),
      http.post('/api/v1/projects/demo/pipelines/main/builds/13/cancel', () =>
        HttpResponse.json({ number: 13, build_id: 1, attempt: 1, status: 'cancelled' }, { status: 202 }),
      ),
      http.post('/api/v1/projects/demo/pipelines/nightly/builds/4/rerun', () =>
        HttpResponse.json({ number: 4, build_id: 1, attempt: 2, status: 'queued' }, { status: 202 }),
      ),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(3))

    await w.find('[data-testid="view-list-btn"]').trigger('click')
    expect(w.find('.pipe-table').exists()).toBe(true)
    const rows = w.findAll('.pipe-row')
    expect(rows.find((r) => r.text().includes('ci'))!.find('.btn-outline.blue').text()).toBe('运行')
    expect(rows.find((r) => r.text().includes('main'))!.find('.btn-outline.red').text()).toBe('终止')
    expect(rows.find((r) => r.text().includes('nightly'))!.find('.btn-outline.orange').text()).toBe('重试')

    requests = []
    // 运行：POST trigger + 行刷新（stats 重取一次）。
    await rows.find((r) => r.text().includes('ci'))!.find('.btn-outline').trigger('click')
    await vi.waitFor(() => {
      expect(requests).toContain('POST /api/v1/projects/demo/pipelines/ci/builds')
      expect(requests.filter((r) => r === 'GET /api/v1/projects/demo/pipelines/ci/stats')).toHaveLength(1)
    })

    // 终止：POST cancel。
    await rows.find((r) => r.text().includes('main'))!.find('.btn-outline.red').trigger('click')
    await vi.waitFor(() =>
      expect(requests).toContain('POST /api/v1/projects/demo/pipelines/main/builds/13/cancel'),
    )

    // 重试：POST rerun from_failed。
    await rows.find((r) => r.text().includes('nightly'))!.find('.btn-outline.orange').trigger('click')
    await vi.waitFor(() =>
      expect(requests).toContain('POST /api/v1/projects/demo/pipelines/nightly/builds/4/rerun'),
    )
  })

  it('收藏入口（W8）：已收藏星标高亮；切换走 PUT/DELETE 收藏端点', async () => {
    mockList([
      { project: 'demo', pipeline: 'main' },
      { project: 'demo', pipeline: 'ci' },
    ])
    mockStats({
      'demo/main': statsBody({ number: 13, status: 'succeeded' }),
      'demo/ci': statsBody({ number: 7, status: 'succeeded' }),
    })
    server.use(
      http.get('/api/v1/user/pipeline-favorites', () =>
        HttpResponse.json([
          { project: 'demo', pipeline: 'main', added_at: 1_700_000_000_000, latest_build: null },
        ]),
      ),
    )
    const mutations: string[] = []
    server.use(
      http.put('/api/v1/user/pipeline-favorites/:project/:pipeline', ({ params }) => {
        mutations.push(`PUT ${String(params.project)}/${String(params.pipeline)}`)
        return new HttpResponse(null, { status: 204 })
      }),
      http.delete('/api/v1/user/pipeline-favorites/:project/:pipeline', ({ params }) => {
        mutations.push(`DELETE ${String(params.project)}/${String(params.pipeline)}`)
        return new HttpResponse(null, { status: 204 })
      }),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(2))

    const mainFav = w.find('[data-testid="fav-demo/main"]')
    expect(mainFav.classes()).toContain('active')
    expect(w.find('[data-testid="fav-demo/ci"]').classes()).not.toContain('active')

    // 收藏 ci：PUT + 星标高亮。
    await w.find('[data-testid="fav-demo/ci"]').trigger('click')
    await vi.waitFor(() => {
      expect(mutations).toContain('PUT demo/ci')
      expect(w.find('[data-testid="fav-demo/ci"]').classes()).toContain('active')
    })

    // 再点取消收藏：DELETE + 高亮消失。
    await w.find('[data-testid="fav-demo/ci"]').trigger('click')
    await vi.waitFor(() => {
      expect(mutations).toContain('DELETE demo/ci')
      expect(w.find('[data-testid="fav-demo/ci"]').classes()).not.toContain('active')
    })
  })

  it('清单端点失败：整页报错 + 重试恢复（P1 契约纪律，不做探测回退）', async () => {
    let calls = 0
    server.use(
      http.get('/api/v1/pipelines', () => {
        calls += 1
        if (calls === 1) {
          return HttpResponse.json({ code: 'INTERNAL', message: '服务内部错误', detail: null }, { status: 500 })
        }
        return HttpResponse.json({
          items: [{ project: 'demo', pipeline: 'main', updated_at: 1_700_000_000_000 }],
          total: 1,
        })
      }),
    )
    mockStats({ 'demo/main': statsBody({ number: 13, status: 'succeeded' }) })

    const w = mountView()
    await vi.waitFor(() => expect(w.find('[role="alert"]').exists()).toBe(true))
    expect(w.find('[data-testid="pipelines-retry"]').exists()).toBe(true)

    await w.find('[data-testid="pipelines-retry"]').trigger('click')
    await vi.waitFor(() => expect(w.findAll('.p-card')).toHaveLength(1))
  })

  it('首载骨架屏（清单未到时无内容、无错误）', async () => {
    server.use(
      http.get('/api/v1/pipelines', () => new Promise<Response>(() => {})),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="pipelines-skeleton"]').exists()).toBe(true))
    expect(w.find('[role="alert"]').exists()).toBe(false)
  })
})

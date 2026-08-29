// 工作台行为测试（原型页一重构，spec #99；数据面仍是 ADR-0019 概览快照）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，票 #101）——组件经真实 http
// client 打 src/mocks handlers，per-test 用 server.use 覆盖概览端点响应。
// 只测外部行为（用户可见状态、DOM 事件、网络请求形态断言）。视图在
// onMounted 即发请求：mount 须在 handler 覆盖之后。
// - 指标卡：在途任务（槽位占用）/ 构建（终态合计 + 成功/失败副标）/
//   队列深度（首要原因副标）/ 在线构建机（可用率）
// - 最近构建行：pipeline #号 + 项目副行、状态徽章、触发、耗时、相对时间；
//   点击行 → 构建详情
// - 右栏 Agent 健康：在线比 + 三类事实警示（异常/正常徽章）；零 Agent 行
// - 右栏 最近流水线：去重前 3；名称 → 构建列表；运行按钮 → POST trigger
// - 快照失败：loadError 报错 + 重试按钮；首载骨架屏

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import OverviewView from '@/views/OverviewView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

/** 一个概览快照响应（最小可被统计消费的形态，全零）。 */
function emptySnapshot(): Record<string, unknown> {
  return {
    queue_depth: 0,
    queue_reasons: [],
    agents_online: 0,
    agents_total: 0,
    slots_used: 0,
    slots_total: 0,
    builds_terminal: { succeeded: 0, failed: 0, cancelled: 0, timeout: 0 },
    artifact_bytes: 0,
    log_bytes: 0,
    alerts: { has_no_match: false, has_offline_agent: false, has_draining_incompatible: false },
    recent_builds: [],
  }
}

/** 覆盖概览端点响应（一次性；未覆盖时回落 mocks 全量 fixture）。 */
function mockOverview(body: Record<string, unknown>, status = 200): void {
  server.use(
    http.get('/api/v1/overview', () => HttpResponse.json(body, { status })),
  )
}

/** 包装组件：NMessageProvider + OverviewView（useMessage 注入可用）。 */
const Host = defineComponent({
  name: 'OverviewHost',
  setup() {
    return () => h(NMessageProvider, () => h(OverviewView))
  },
})

describe('OverviewView 工作台（指标卡 + 最近构建 + 右栏）', () => {
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
        {
          path: '/projects/:name/pipelines/:pipeline/builds/:number',
          name: 'build-detail',
          component: { template: '<div />' },
        },
      ],
    })
    await router.push('/')
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

  it('指标卡四张同排：在途任务/构建/队列深度/Agent 健康（GET /overview 单一来源）', async () => {
    mockOverview({
      ...emptySnapshot(),
      agents_online: 1,
      agents_total: 2,
      slots_used: 1,
      slots_total: 2,
      queue_depth: 0,
      builds_terminal: { succeeded: 5, failed: 1, cancelled: 2, timeout: 0 },
    })

    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('在途任务'))
    // 在途任务 = 槽位占用 1/2，使用率 50%。
    expect(w.text()).toContain('1')
    expect(w.text()).toContain('/ 2')
    expect(w.text()).toContain('使用率 50%')
    // 构建 = 终态合计 8，副标 成功 5 · 失败 1。
    expect(w.text()).toContain('成功 5 · 失败 1')
    // 队列 0 → 空闲副标。
    expect(w.text()).toContain('空闲，无排队')
    // 顶部指标卡行 4 张：在途任务/构建/队列深度/Agent 健康（同排一行）。
    expect(w.find('section[aria-label="metrics"]').findAll('.metric-card')).toHaveLength(4)
    // Agent 健康卡：在线 1/2 台 + 无异常 → 单枚「全部正常」。
    expect(w.text()).toContain('/ 2台')
    expect(w.findAll('.health-badges .badge').map((b) => b.text())).toEqual(['全部正常'])

    // 请求形态：GET /api/v1/overview（唯一数据源，经真实 http client 打 MSW）。
    await vi.waitFor(() => expect(requests).toEqual(['GET /api/v1/overview']))
  })

  it('队列深度副标：有排队给首要原因（queue_reasons[0]）', async () => {
    mockOverview({
      ...emptySnapshot(),
      queue_depth: 2,
      queue_reasons: [
        { reason: 'no_online_agent', depth: 1 },
        { reason: 'missing_labels', depth: 1 },
      ],
    })

    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('队列深度'))
    expect(w.text()).toContain('首要原因：等待匹配 agent：无在线 agent')
  })

  it('最近构建行：pipeline #号/项目/状态徽章/触发/耗时/相对时间；点击行 → 构建详情', async () => {
    mockOverview({
      ...emptySnapshot(),
      recent_builds: [
        {
          project: 'demo',
          pipeline: 'release',
          number: 12,
          status: 'succeeded',
          trigger: 'manual',
          started_at: 1_700_000_000_000,
          finished_at: 1_700_000_060_000,
        },
      ],
    })

    const w = mountView()
    await vi.waitFor(() => expect(w.find('.run-row').exists()).toBe(true))
    expect(w.find('.run-row').text()).toContain('release')
    expect(w.find('.run-row').text()).toContain('#12')
    expect(w.find('.run-row').text()).toContain('demo')
    expect(w.find('.run-row .badge').text()).toBe('成功')
    expect(w.find('.run-row').text()).toContain('手动')
    expect(w.find('.run-row').text()).toContain('1m 0s')
    // 固定历史时间戳 → 相对时间「n 天前」。
    expect(w.find('.run-row').text()).toContain('天前')

    const pushSpy = vi.spyOn(router, 'push')
    await w.find('.run-row').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'build-detail',
      params: { name: 'demo', pipeline: 'release', number: '12' },
    })
  })

  it('Agent 健康卡（与指标同行）：在线比 + 事实警示徽章（异常/正常）', async () => {
    mockOverview({
      ...emptySnapshot(),
      agents_online: 1,
      agents_total: 1,
      alerts: {
        has_no_match: true,
        has_offline_agent: true,
        has_draining_incompatible: false,
      },
    })

    const w = mountView()
    // 健康卡在线 1/1 台（全部在线 → 数值转绿类）。
    await vi.waitFor(() => expect(w.find('.health-card .metric-value').classes()).toContain('green'))
    expect(w.text()).toContain('/ 1台')
    // 只亮异常事实（红徽章 + 完整句 title 提示）：离线/无匹配，无排空异常。
    const badges = w.findAll('.health-badges .badge')
    expect(badges.map((b) => b.text())).toEqual(['离线 Agent', '无匹配任务'])
    expect(badges.every((b) => b.classes().includes('failed'))).toBe(true)
    expect(badges[0]!.attributes('title')).toContain('有 Agent 离线')
    // 无整页 alert（事实警示进健康卡，非 NAlert）。
    expect(w.find('[role="alert"]').exists()).toBe(false)
  })

  it('零 Agent：健康卡给「尚未注册构建机」行（不再用 NAlert info）', async () => {
    mockOverview(emptySnapshot())

    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('尚未注册构建机'))
    expect(w.find('.n-alert').exists()).toBe(false)
  })

  it('最近流水线卡：去重取前 3；名称 → 构建列表；运行按钮 → POST trigger', async () => {
    mockOverview({
      ...emptySnapshot(),
      recent_builds: [
        {
          project: 'demo',
          pipeline: 'release',
          number: 12,
          status: 'succeeded',
          trigger: 'manual',
          started_at: 1_700_000_000_000,
          finished_at: 1_700_000_060_000,
        },
        {
          project: 'demo',
          pipeline: 'release',
          number: 11,
          status: 'succeeded',
          trigger: 'manual',
          started_at: 1_699_000_000_000,
          finished_at: 1_699_000_060_000,
        },
        {
          project: 'demo',
          pipeline: 'nightly',
          number: 3,
          status: 'failed',
          trigger: 'cron',
          started_at: 1_700_000_000_000,
          finished_at: 1_700_000_060_000,
        },
      ],
    })
    server.use(
      http.post('/api/v1/projects/demo/pipelines/release/builds', () =>
        HttpResponse.json({ number: 13, build_id: 1, attempt: 1, status: 'queued' }, { status: 202 }),
      ),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.fav-row')).toHaveLength(2))
    // release 去重保留最近一次（#12）。
    expect(w.findAll('.fav-row')[0]!.text()).toContain('release')
    expect(w.findAll('.fav-row')[0]!.text()).toContain('#12')
    expect(w.findAll('.fav-row')[1]!.text()).toContain('nightly')

    // 名称 → 构建列表。
    const pushSpy = vi.spyOn(router, 'push')
    await w.findAll('.fav-row')[0]!.find('.fav-name').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'build-list',
      params: { name: 'demo', pipeline: 'release' },
    })

    // 运行按钮 → POST /api/v1/projects/demo/pipelines/release/builds（空参数体）。
    await w.findAll('.fav-row')[0]!.find('.btn-outline').trigger('click')
    await vi.waitFor(() => {
      expect(requests).toContain('POST /api/v1/projects/demo/pipelines/release/builds')
    })
  })

  it('快照失败：整页报错 + 重试；重试成功后恢复', async () => {
    // server.use 的 handler 持续生效（非一次性）：首请求 500，之后恢复 200。
    let overviewCalls = 0
    server.use(
      http.get('/api/v1/overview', () => {
        overviewCalls += 1
        if (overviewCalls === 1) {
          return HttpResponse.json(
            { code: 'INTERNAL', message: '服务内部错误', detail: null },
            { status: 500 },
          )
        }
        return HttpResponse.json(emptySnapshot())
      }),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="overview-error"]').exists()).toBe(true))

    // 重试：点击按钮后重新请求并恢复展示（零 Agent 行出现 = 恢复）。
    await w.find('[data-testid="overview-error"] button').trigger('click')
    await vi.waitFor(() => expect(w.text()).toContain('尚未注册构建机'))
  })

  it('首载骨架屏（数据未到时无内容、无错误）', async () => {
    server.use(
      http.get('/api/v1/overview', () => new Promise<Response>(() => {})),
    )

    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="overview-skeleton"]').exists()).toBe(true))
    expect(w.find('[role="alert"]').exists()).toBe(false)
  })

  it('「查看全部」链接指向流水线页', async () => {
    mockOverview(emptySnapshot())

    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('查看全部'))
    const link = w.findAll('a').find((a) => a.text() === '查看全部')
    expect(link).toBeDefined()
    expect(link!.attributes('href')).toBe('/pipelines')
  })
})

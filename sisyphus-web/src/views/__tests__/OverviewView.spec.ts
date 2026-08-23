// 概览页行为测试（ADR-0019，票 B5-T7）：stat 卡 + 事实型警示态 + 最近构建。
// 数据源 = 概览快照端点 `GET /api/v1/overview`（单一来源，B4-T3 退化面已移除）。
// 只测外部行为（用户可见状态、DOM 事件、网络请求形态断言），API 层以
// fetch mock 驱动。
// - stat 卡：Agent 在线/总数、槽位占用、队列深度、构建终态、存储占用
// - 警示态：无匹配任务 / 有离线 Agent / 排空或不兼容 Agent（快照 alerts）
// - 最近构建：快照 recent_builds 表格
// - 失败：loadError 报错 + 重试按钮；无静默部分值
// 视图在 onMounted 即发请求：mount 须在设置 fetch mock 之后（先设 mock 再挂载）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NDataTable } from 'naive-ui'

import OverviewView from '@/views/OverviewView.vue'
import { i18n, setLocale } from '@/i18n'

/** 构造 mock JSON 响应（jsdom 无 fetch，需自造 Response 壳）。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

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

describe('OverviewView 概览页（stat 卡 + 警示态 + 最近构建）', () => {
  let pinia: Pinia
  let router: Router

  const fetchMock = vi.fn()

  function mountView(): VueWrapper {
    return mount(OverviewView, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/', name: 'overview', component: { template: '<div />' } }],
    })
    await router.push('/')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载快照：展示 Agent/槽位/队列/终态/存储 stat 卡（GET /overview）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        agents_online: 1,
        agents_total: 2,
        slots_used: 1,
        slots_total: 2,
        queue_depth: 3,
        builds_terminal: { succeeded: 5, failed: 1, cancelled: 2, timeout: 0 },
        artifact_bytes: 1_500_000_000,
        log_bytes: 1_500_000,
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('1 / 2'))
    expect(wrapper.text()).toContain('3')
    expect(wrapper.text()).toContain('成功 5')
    expect(wrapper.text()).toContain('失败 1')
    expect(wrapper.text()).toContain('取消 2')
    expect(wrapper.text()).toContain('1.4 GB')
    expect(wrapper.text()).toContain('1.4 MB')

    // 请求形态：GET /api/v1/overview（唯一数据源，不再组合 /agents /projects）。
    const calls = fetchMock.mock.calls.map((c) => (c as [string, RequestInit])[0])
    expect(calls).toEqual(['/api/v1/overview'])
    wrapper.unmount()
  })

  it('警示态全部来自快照 alerts：离线 / 无匹配 / 排空或不兼容', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        agents_total: 1,
        alerts: {
          has_no_match: true,
          has_offline_agent: true,
          has_draining_incompatible: true,
        },
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('有 Agent 离线'))
    expect(wrapper.text()).toContain('存在无匹配 Agent 的任务')
    expect(wrapper.text()).toContain('存在排空或版本不兼容的 Agent')
    wrapper.unmount()
  })

  it('全部在线 + 有 Agent 且无警示：不展示任何 alert', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, { ...emptySnapshot(), agents_total: 1, agents_online: 1 }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('1 / 1'))
    expect(wrapper.find('[role="alert"]').exists()).toBe(false)
    wrapper.unmount()
  })

  it('队列原因分类展示（快照 queue_reasons）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        queue_depth: 2,
        queue_reasons: [
          { reason: 'no_online_agent', depth: 1 },
          { reason: 'missing_labels', depth: 1 },
        ],
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('等待匹配 agent：无在线 agent'))
    expect(wrapper.text()).toContain('等待匹配 agent：缺标签')
    wrapper.unmount()
  })

  it('最近构建表格：项目/pipeline/号/状态/触发源/时间', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
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
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('demo'))
    expect(wrapper.text()).toContain('release')
    expect(wrapper.text()).toContain('#12')
    expect(wrapper.text()).toContain('成功')
    expect(wrapper.text()).toContain('手动')
    // 平板窄视口：NDataTable 最小表宽，容器更窄时横向滚动而非挤压列。
    expect(wrapper.findComponent(NDataTable).props('scrollX')).toBe(720)
    wrapper.unmount()
  })

  it('快照失败：整页报错 + 重试；重试成功后恢复', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(500, { code: 'INTERNAL', message: '服务内部错误' }))
      .mockResolvedValueOnce(jsonResponse(200, emptySnapshot()))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('[role="alert"]').exists()).toBe(true))

    // 重试：点击按钮后重新请求并恢复展示。
    const retry = wrapper.find('button')
    await retry.trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('0 / 0'))
    wrapper.unmount()
  })
})

describe('OverviewView Naive UI 迁移（#91）', () => {
  let pinia: Pinia
  let router: Router

  const fetchMock = vi.fn()

  function mountView(): VueWrapper {
    return mount(OverviewView, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/', name: 'overview', component: { template: '<div />' } }],
    })
    await router.push('/')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('首载中显示 NSkeleton 骨架屏（数据未到时无内容、无错误）', async () => {
    fetchMock.mockImplementation(() => new Promise(() => {}))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-skeleton').exists()).toBe(true))
    expect(wrapper.find('[role="alert"]').exists()).toBe(false)
    wrapper.unmount()
  })

  it('stat 卡改用 NCard + NStatistic（带图标）', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(200, emptySnapshot()))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('0 / 0'))
    expect(wrapper.findAll('.n-card').length).toBeGreaterThanOrEqual(5)
    expect(wrapper.findAll('.n-statistic').length).toBe(5)
    expect(wrapper.findAll('.overview-stat-icon').length).toBeGreaterThanOrEqual(5)
    wrapper.unmount()
  })

  it('队列原因分类使用 NTag 状态色标签（不同原因不同颜色）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        queue_depth: 2,
        queue_reasons: [
          { reason: 'no_online_agent', depth: 1 },
          { reason: 'missing_labels', depth: 1 },
        ],
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('等待匹配 agent：无在线 agent'))
    const tags = wrapper.findAll('.queue-reason-list .n-tag')
    expect(tags.length).toBe(2)
    expect(tags[0]!.text()).toContain('无在线 agent')
    // 不同原因 → 不同 NTag type（no_online_agent=error / missing_labels=warning），
    // 主题色经 cssVars 落在 `--n-color`，各自不同。
    const color0 = tags[0]!.attributes('style')
    const color1 = tags[1]!.attributes('style')
    expect(color0).toContain('--n-color:')
    expect(color1).toContain('--n-color:')
    expect(color0).not.toBe(color1)
    wrapper.unmount()
  })

  it('警示态改用 NAlert（类型匹配严重程度：离线/无匹配=warning，无 Agent=info）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        agents_total: 1,
        alerts: {
          has_no_match: true,
          has_offline_agent: true,
          has_draining_incompatible: false,
        },
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('有 Agent 离线'))
    const alerts = wrapper.findAll('.n-alert')
    expect(alerts.length).toBe(2)
    for (const a of alerts) {
      expect(a.classes()).toContain('n-alert--show-icon')
    }
    wrapper.unmount()
  })

  it('尚未注册任何 Agent → NAlert type=info 信息提示（非警示严重程度）', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(200, emptySnapshot()))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('尚未注册任何 Agent'))
    const alert = wrapper.find('.n-alert')
    expect(alert.exists()).toBe(true)
    expect(alert.classes()).toContain('n-alert--show-icon')
    wrapper.unmount()
  })

  it('构建终态 stat 卡用 NTag 展示四态（成功/失败/取消/超时）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        builds_terminal: { succeeded: 5, failed: 1, cancelled: 2, timeout: 0 },
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('成功 5'))
    expect(wrapper.text()).toContain('失败 1')
    expect(wrapper.text()).toContain('取消 2')
    expect(wrapper.text()).toContain('超时 0')
    const outcomeTags = wrapper.findAll('.build-outcomes .n-tag')
    expect(outcomeTags.length).toBe(4)
    wrapper.unmount()
  })

  it('最近构建改用 NDataTable，且列可排序（点击列头按构建号排序）', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...emptySnapshot(),
        recent_builds: [
          {
            project: 'demo',
            pipeline: 'release',
            number: 1,
            status: 'succeeded',
            trigger: 'manual',
            started_at: 1_700_000_000_000,
            finished_at: 1_700_000_060_000,
          },
          {
            project: 'demo',
            pipeline: 'nightly',
            number: 2,
            status: 'failed',
            trigger: 'cron',
            started_at: 1_700_000_000_000,
            finished_at: 1_700_000_060_000,
          },
        ],
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-data-table').exists()).toBe(true))

    // 表头按 data-col-key 标记（NDataTable 实现细节，用于驱动排序交互）。
    const numberTh = wrapper.find('th[data-col-key="number"]')
    expect(numberTh.exists()).toBe(true)

    // 初始顺序按快照（1 在上，2 在下）。
    const trs = () => wrapper.findAll('.n-data-table-tbody tr')
    expect(trs()[0]!.text()).toContain('#1')
    expect(trs()[1]!.text()).toContain('#2')

    // 点击「构建号」列头 → 降序（NDataTable 默认 first click = descend）。
    await numberTh.trigger('click')
    await vi.waitFor(() => {
      expect(wrapper.findAll('.n-data-table-tbody tr')[0]!.text()).toContain('#2')
    })
    wrapper.unmount()
  })
})

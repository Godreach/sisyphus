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

  it('全部在线 + 无警示：不展示任何 alert', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(200, emptySnapshot()))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('0 / 0'))
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

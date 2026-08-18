// 概览页行为测试（ADR-0019，票 B4-T3）：stat 卡 + 事实型警示态 + 最近构建。
// 只测外部行为（用户可见状态、DOM 事件、网络请求形态断言），API 层以
// fetch mock 驱动。
// - Agent 在线/总数 + 离线警示：GET /agents（全局 admin 专属）
// - 项目数：GET /projects（可见性过滤）
// - 退化标注：队列深度/构建终态/最近构建/其余警示依赖概览快照端点，未交付
//   → 显式「依赖概览快照端点」标注
// - 普通用户（/agents 403）→ Agent 卡「仅全局管理员可见」退化，不报错
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

/** 一个 Agent 行（最小可被统计消费的形态）。 */
function agent(name: string, online: boolean, disabled = false) {
  return {
    name,
    online,
    disabled,
    system_labels: [],
    custom_labels: [],
    max_concurrency: 1,
    active_jobs: 0,
    last_seen_at: null,
    created_at: 0,
    updated_at: 0,
  }
}

/** 一个项目行（最小形态）。 */
function proj(name: string) {
  return { id: 1, name, scm_type: 'git', scm_url: 'x', default_branch: null, created_at: 0, updated_at: 0 }
}

describe('OverviewView 概览页（stat 卡 + 警示态 + 退化标注）', () => {
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

  it('加载后展示 Agent 在线/总数 + 项目数 stat 卡（GET /agents + GET /projects）', async () => {
    // 2 个 Agent（1 在线、1 启用离线）+ 1 个项目。
    fetchMock
      .mockResolvedValueOnce(jsonResponse(200, [agent('a-1', true), agent('a-2', false)]))
      .mockResolvedValueOnce(jsonResponse(200, [proj('p1')]))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('1 / 2'))
    expect(wrapper.text()).toContain('有 Agent 离线')
    expect(wrapper.text()).toContain('项目')

    // 请求形态：GET /api/v1/agents + GET /api/v1/projects。
    const calls = fetchMock.mock.calls.map((c) => (c as [string, RequestInit])[0])
    expect(calls).toContain('/api/v1/agents')
    expect(calls).toContain('/api/v1/projects')
    wrapper.unmount()
  })

  it('全部 Agent 在线：不展示离线警示', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(200, [agent('a-1', true)]))
      .mockResolvedValueOnce(jsonResponse(200, []))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('1 / 1'))
    expect(wrapper.find('[role="alert"]').exists()).toBe(false)
    wrapper.unmount()
  })

  it('普通用户（/agents 403）→ Agent 卡「仅全局管理员可见」退化，不报错；项目数仍展示', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
      .mockResolvedValueOnce(jsonResponse(200, [proj('p1')]))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    expect(wrapper.text()).toContain('项目')
    expect(wrapper.find('[role="alert"]').exists()).toBe(false)
    wrapper.unmount()
  })

  it('队列深度/构建终态 stat 卡显式标注「依赖概览快照端点」（退化态）', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(200, []))
      .mockResolvedValueOnce(jsonResponse(200, []))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('Agent 在线'))
    // 退化卡 + 最近构建退化标注：不静默给假值。
    expect(wrapper.text()).toContain('依赖概览快照端点')
    expect(wrapper.text()).toContain('最近构建')
    expect(wrapper.text()).toContain('全局最近构建依赖构建列表端点')
    wrapper.unmount()
  })

  it('零 Agent：展示「尚未注册任何 Agent」信息警示', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(200, []))
      .mockResolvedValueOnce(jsonResponse(200, []))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('尚未注册任何 Agent'))
    wrapper.unmount()
  })
})

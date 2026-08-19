// Agent 详情行为测试（ADR-0008/0011/0012/0019，票 B4-T5）：只测外部行为，API
// 层以 fetch mock 驱动。详情页是「看」的表面：标签分区、槽位占用、磁盘三口径、
// 工作区/缓存清理占位（端点未交付 → 退化标注）。
// 视图在 onMounted 即发详情请求：mount 须在设置 fetch mock 之后。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import AgentDetailView from '@/views/AgentDetailView.vue'
import { i18n, setLocale } from '@/i18n'
import type { AgentResponse } from '@/api/types'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

function agent(name: string, overrides: Partial<AgentResponse> = {}): AgentResponse {
  return {
    name,
    online: true,
    disabled: false,
    system_labels: ['sisyphus/os=linux', 'sisyphus/arch=amd64'],
    custom_labels: ['region=cn'],
    max_concurrency: 2,
    active_jobs: 1,
    last_seen_at: 1_700_000_000_000,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

describe('AgentDetailView 详情（标签分区 + 槽位 + 磁盘三口径 + 清理占位）', () => {
  let pinia: Pinia
  let router: Router

  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    let best: { len: number; res: Response } | null = null
    for (const [prefix, res] of routes) {
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best
      ? best.res
      : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
  })

  function setRoute(prefix: string, res: Response): void {
    routes.set(prefix, res)
  }

  function mountView(): VueWrapper {
    return mount(AgentDetailView, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/agents', name: 'agents', component: { template: '<div />' } },
        { path: '/agents/:name', name: 'agent-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/agents/demo')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载详情：系统/自定义标签分区 + 槽位占用 + 返回链接', async () => {
    setRoute('/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.agent-state-badge').exists()).toBe(true))

    expect(wrapper.text()).toContain('sisyphus/os=linux')
    expect(wrapper.text()).toContain('sisyphus/arch=amd64')
    expect(wrapper.text()).toContain('region=cn')
    // 槽位占用：在途 1 / 槽位 2。
    expect(wrapper.text()).toContain('在途任务')
    expect(wrapper.text()).toContain('1')
    expect(wrapper.text()).toContain('并发槽位')
    expect(wrapper.text()).toContain('2')
    // 返回链接。
    expect(wrapper.get('.agent-back').text()).toContain('返回 Agent 列表')
    const [url] = fetchMock.mock.calls[0] as [string]
    expect(url).toBe('/api/v1/agents/demo')
    wrapper.unmount()
  })

  it('磁盘三口径：卷级 total/free + 缓存占用 + 工作区占用（formatBytes）', async () => {
    setRoute(
      '/api/v1/agents/demo',
      jsonResponse(
        200,
        agent('demo', {
          disk_usage: {
            volumes: [
              { mount_point: '/', total_bytes: 100_000, free_bytes: 40_000 },
              { mount_point: 'D:', total_bytes: 200_000, free_bytes: 10_000 },
            ],
            cache_bytes: 5_000,
            workspace_bytes: 8_000,
          },
        }),
      ),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.disk-table tbody tr').length).toBe(2))

    // 卷级表：挂载点 + total/free（formatBytes 归一）。
    const rows = wrapper.findAll('.disk-table tbody tr')
    expect(rows[0]!.text()).toContain('/')
    expect(rows[0]!.text()).toContain('97.7 KB') // total 100_000
    expect(rows[0]!.text()).toContain('39.1 KB') // free 40_000
    expect(rows[1]!.text()).toContain('D:')

    // 缓存占用 + 工作区占用。
    expect(wrapper.text()).toContain('缓存占用')
    expect(wrapper.text()).toContain('4.9 KB') // cache 5_000
    expect(wrapper.text()).toContain('工作区占用')
    expect(wrapper.text()).toContain('7.8 KB') // workspace 8_000
    wrapper.unmount()
  })

  it('disk_usage 为空 → 从未上报磁盘占用', async () => {
    setRoute('/api/v1/agents/demo', jsonResponse(200, agent('demo', { disk_usage: null })))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.agent-state-badge').exists()).toBe(true))
    expect(wrapper.text()).toContain('从未上报磁盘占用')
    expect(wrapper.find('.disk-table').exists()).toBe(false)
    wrapper.unmount()
  })

  it('标签为空 → （无）占位', async () => {
    setRoute(
      '/api/v1/agents/demo',
      jsonResponse(200, agent('demo', { system_labels: [], custom_labels: [] })),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.agent-state-badge').exists()).toBe(true))
    expect(wrapper.findAll('.form-hint').some((e) => e.text() === '（无）')).toBe(true)
    wrapper.unmount()
  })

  it('工作区/缓存清理入口：动作区占位（禁用按钮）+ 端点未交付退化标注', async () => {
    setRoute('/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.agent-state-badge').exists()).toBe(true))

    // 两个清理按钮（工作区 / 缓存）均禁用（端点未交付）。
    const cleanupBtns = wrapper.findAll('.cleanup-action')
    expect(cleanupBtns.length).toBe(2)
    for (const btn of cleanupBtns) {
      expect((btn.element as HTMLButtonElement).disabled).toBe(true)
    }
    expect(wrapper.text()).toContain('工作区清理')
    expect(wrapper.text()).toContain('缓存清理')
    // 退化标注：清理指令端点尚未交付。
    expect(wrapper.text()).toContain('清理指令端点尚未交付')
    wrapper.unmount()
  })

  it('404 → Agent 不存在', async () => {
    setRoute(
      '/api/v1/agents/demo',
      jsonResponse(404, { code: 'NOT_FOUND', message: 'Agent demo 不存在' }),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('Agent 不存在'))
    wrapper.unmount()
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染详情体', async () => {
    setRoute(
      '/api/v1/agents/demo',
      jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    // 不渲染徽标 / 详情体（403 退化，不报错）。
    expect(wrapper.find('.agent-state-badge').exists()).toBe(false)
    expect(wrapper.find('.label-chips').exists()).toBe(false)
    wrapper.unmount()
  })

  it('停用 Agent 详情：徽标展示「停用」', async () => {
    setRoute(
      '/api/v1/agents/demo',
      jsonResponse(200, agent('demo', { online: true, disabled: true })),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.agent-state-badge').exists()).toBe(true))
    const badge = wrapper.get('.agent-state-badge')
    expect(badge.text()).toBe('停用')
    expect(badge.classes()).toContain('agent-state-disabled')
    wrapper.unmount()
  })
})

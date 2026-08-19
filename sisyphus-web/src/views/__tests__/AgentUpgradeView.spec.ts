// Agent 升级页行为测试（ADR-0017，票 B4-T6）：升级端点未交付 → 形态搭好 +
// 动作区占位。只测外部行为，API 层以 fetch mock 驱动。视图在 onMounted 即发
// Agent 列表请求：mount 须在设置 fetch mock 之后。
// - Agent 清单 + 排空/升级阶段两列退化标「—」+ 退化标注
// - 升级动作区占位：上传/全量/单台按钮禁用 + 退化标注
// - 包选择：记文件名展示（不上送、不发请求）
// - 403 → admin-only 退化态；无 Agent → 空态

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import AgentUpgradeView from '@/views/AgentUpgradeView.vue'
import { i18n, setLocale } from '@/i18n'
import type { AgentResponse } from '@/api/types'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

function agent(name: string, overrides: Partial<AgentResponse> = {}): AgentResponse {
  return {
    name,
    online: false,
    disabled: false,
    system_labels: ['sisyphus/os=linux'],
    custom_labels: [],
    max_concurrency: 1,
    active_jobs: 0,
    last_seen_at: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

describe('AgentUpgradeView 形态搭好 + 动作区占位', () => {
  let pinia: Pinia
  let router: Router

  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = (init?.method ?? 'GET').toUpperCase()
    let best: { len: number; res: Response } | null = null
    for (const [key, res] of routes) {
      const [m, prefix] = [key.slice(0, key.indexOf(' ')), key.slice(key.indexOf(' ') + 1)]
      if (method !== m.toUpperCase()) continue
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best
      ? best.res
      : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${method} ${url}` })
  })

  function setRoute(method: string, prefix: string, res: Response): void {
    routes.set(`${method.toUpperCase()} ${prefix}`, res)
  }

  function mountView(): VueWrapper {
    return mount(AgentUpgradeView, { global: { plugins: [pinia, router, i18n] } })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/upgrade', name: 'admin-upgrade', component: { template: '<div />' } }],
    })
    await router.push('/admin/upgrade')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载 Agent 清单 + 排空/升级阶段两列退化标「—」+ 退化标注', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.upgrade-table tbody tr').length).toBe(1))

    // 排空 / 升级阶段两列今日标「—」（字段未进 REST 契约）。
    const cells = wrapper.findAll('.upgrade-table tbody tr')[0]!.findAll('td')
    expect(cells[2]!.text()).toBe('—')
    expect(cells[3]!.text()).toBe('—')
    // 退化标注在位。
    expect(wrapper.text()).toContain('排空标志与升级阶段字段未进 REST 契约')
    wrapper.unmount()
  })

  it('升级动作区占位：上传/全量/单台按钮均禁用 + 退化标注', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1')]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    // 顶部退化标注：升级端点未交付。
    expect(wrapper.text()).toContain('升级端点尚未交付')
    // 三个动作按钮均禁用（占位）。
    expect(wrapper.get('button[name="upgrade-upload"]').attributes('disabled')).toBeDefined()
    expect(wrapper.get('button[name="upgrade-all"]').attributes('disabled')).toBeDefined()
    expect(wrapper.get('button[name="upgrade-one"]').attributes('disabled')).toBeDefined()
    wrapper.unmount()
  })

  it('包选择：记文件名展示，不上送、不发任何请求', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1')]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    const callsBefore = fetchMock.mock.calls.length
    const input = wrapper.get('input[name="upgrade-package"]').element as HTMLInputElement
    const file = new File(['x'], 'agent-1.0.0-x86_64.tar', { type: 'application/octet-stream' })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await wrapper.get('input[name="upgrade-package"]').trigger('change')

    // 展示已选包名；无新请求发出（上传端点未交付，不上送）。
    await vi.waitFor(() => expect(wrapper.text()).toContain('agent-1.0.0-x86_64.tar'))
    expect(fetchMock.mock.calls.length).toBe(callsBefore)
    wrapper.unmount()
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染动作/表格', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    expect(wrapper.find('.upgrade-section').exists()).toBe(false)
    wrapper.unmount()
  })

  it('无 Agent → 空态提示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))
    wrapper.unmount()
  })
})

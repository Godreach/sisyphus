// Agent 升级页行为测试（ADR-0017，票 B5-T4）：升级端点已交付——包上传 +
// 全量/单台升级指令 + 排空/升级阶段列真值。只测外部行为，API 层以 fetch
// mock 驱动。视图在 onMounted 即发 Agent 清单 + 升级包清单请求：mount 须在
// 设置 fetch mock 之后。
// - 包上传：选文件 + 上传 → POST /upgrade-packages（X-Sisyphus-Filename 头 + body）
// - 全量升级：选包 + 按钮 → POST /agents/upgrade → 受理摘要
// - 单台升级：选 Agent + 选包 + 按钮 → POST /agents/{name}/upgrade
// - 排空/升级阶段列取 Agent 清单真值（draining / upgrade_phase）
// - 403 → admin-only 退化态；无 Agent → 空态

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import AgentUpgradeView from '@/views/AgentUpgradeView.vue'
import { i18n, setLocale } from '@/i18n'
import type { AgentResponse, UpgradePackageResponse } from '@/api/types'

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
    agent_version: null,
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

function pkg(name: string, overrides: Partial<UpgradePackageResponse> = {}): UpgradePackageResponse {
  return {
    package_name: name,
    version: { major: 1, minor: 0, patch: 0 },
    target_os: 'linux',
    target_arch: 'x86_64',
    size: 100,
    sha256: 'abc',
    created_at: 0,
    ...overrides,
  }
}

describe('AgentUpgradeView 升级端点已交付', () => {
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

  it('加载 Agent 清单 + 升级包清单（mount 即两个 GET）', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, [pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.upgrade-table tbody tr').length).toBe(2))

    // 升级包清单区 + Agent 状态区各一表。
    expect(wrapper.text()).toContain('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')
    expect(wrapper.text()).toContain('linux-1')
    // 两个 GET 都发出。
    const urls = fetchMock.mock.calls.map((c) => String(c[0]))
    expect(urls.some((u) => u === '/api/v1/agents')).toBe(true)
    expect(urls.some((u) => u === '/api/v1/upgrade-packages')).toBe(true)
    wrapper.unmount()
  })

  it('排空/升级阶段列取真值：draining=true → 是，upgrade_phase=downloading → 下载中', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, draining: true, upgrade_phase: 'downloading' })]),
    )
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    // Agent 状态表（第二张表）的行：排空列 = 是、阶段列 = 下载中。
    const agentRows = wrapper.findAll('.upgrade-section').at(-1)!.findAll('tbody tr')
    const cells = agentRows[0]!.findAll('td')
    // 列序：Agent / 状态 / 版本 / 排空 / 升级阶段
    expect(cells[3]!.text()).toBe('是')
    expect(cells[4]!.text()).toContain('下载中')
    wrapper.unmount()
  })

  it('包上传：选文件 + 上传 → POST /upgrade-packages（X-Sisyphus-Filename 头 + body）', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1')]))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/upgrade-packages',
      jsonResponse(201, pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    const input = wrapper.get('input[name="upgrade-package"]').element as HTMLInputElement
    const file = new File(['x'], 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz', { type: 'application/octet-stream' })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await wrapper.get('input[name="upgrade-package"]').trigger('change')

    await wrapper.get('button[name="upgrade-upload"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('已上传'))

    const uploadCall = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/upgrade-packages' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(uploadCall).toBeTruthy()
    expect(uploadCall![1]!.headers).toMatchObject({ 'X-Sisyphus-Filename': 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz' })
    expect(uploadCall![1]!.body).toBe(file)
    wrapper.unmount()
  })

  it('全量升级：选包 + 按钮 → POST /agents/upgrade → 受理摘要', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1')]))
    setRoute(
      'GET',
      '/api/v1/upgrade-packages',
      jsonResponse(200, [pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')]),
    )
    setRoute(
      'POST',
      '/api/v1/agents/upgrade',
      jsonResponse(202, { package_name: 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz', issued: 1, skipped: 0 }),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('sisyphus-agent-1.0.0-linux-x86_64.tar.gz'))

    await wrapper.get('button[name="upgrade-all"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('已下发升级指令'))

    const call = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/agents/upgrade' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(call).toBeTruthy()
    expect(JSON.parse(call![1]!.body as string)).toEqual({ package_name: 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz' })
    wrapper.unmount()
  })

  it('单台升级：选 Agent + 选包 + 按钮 → POST /agents/{name}/upgrade', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    setRoute(
      'GET',
      '/api/v1/upgrade-packages',
      jsonResponse(200, [pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')]),
    )
    setRoute(
      'POST',
      '/api/v1/agents/linux-1/upgrade',
      jsonResponse(202, agent('linux-1', { online: true, draining: true, upgrade_phase: 'draining' })),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    await wrapper.get('button[name="upgrade-one"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('已向 linux-1 下发升级指令'))

    expect(
      fetchMock.mock.calls.some(
        (c) => String(c[0]) === '/api/v1/agents/linux-1/upgrade' && (c[1]?.method ?? 'POST') === 'POST',
      ),
    ).toBe(true)
    wrapper.unmount()
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染动作/表格', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    expect(wrapper.find('.upgrade-section').exists()).toBe(false)
    wrapper.unmount()
  })

  it('无 Agent → 空态提示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))
    wrapper.unmount()
  })
})

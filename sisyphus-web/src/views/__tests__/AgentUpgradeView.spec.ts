// Agent 升级页行为测试（ADR-0017，票 B5-T4）：升级端点已交付——包上传 +
// 全量/单台升级指令 + 排空/升级阶段列真值。只测外部行为，API 层以 fetch
// mock 驱动。视图在 onMounted 即发 Agent 清单 + 升级包清单请求：mount 须在
// 设置 fetch mock 之后。
// - 包上传：NUpload 选文件即传 → POST /upgrade-packages（X-Sisyphus-Filename
//   头 + body；NUpload 内部 input[type=file] 注入 files + change 驱动）
// - 全量升级：选包 + 按钮 → POST /agents/upgrade → 受理摘要
// - 单台升级：选 Agent + 选包 + 按钮 → POST /agents/{name}/upgrade
// - 排空/升级阶段列取 Agent 清单真值（draining / upgrade_phase → NProgress）
// #95: NDataTable 迁移后行经 .n-data-table-tr 定位；成功反馈 NMessage
// teleport 到 body——文案经 document.body 断言（与 AgentListView.spec 同纪律）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NDataTable, NMessageProvider, NProgress, NTag } from 'naive-ui'
import { defineComponent, h } from 'vue'

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
    cpu_usage: 20,
    memory_usage: 35,
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

/** 包装组件：NMessageProvider + AgentUpgradeView，保证 useMessage 注入可用。 */
const UpgradeWrapper = defineComponent({
  name: 'UpgradeWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AgentUpgradeView, { ...attrs }))
  },
})

describe('AgentUpgradeView 升级端点已交付', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

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
    wrapper = mount(UpgradeWrapper, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
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
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  it('加载 Agent 清单 + 升级包清单（mount 即两个 GET，两表齐渲染）', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, [pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')]))
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-packages-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // 升级包清单区 + Agent 状态区各一表。
    expect(w.text()).toContain('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')
    expect(w.text()).toContain('linux-1')
    // 平板窄视口：两表各设最小表宽（包清单 / Agent 状态），容器更窄时横向滚动。
    const scrollXs = w.findAllComponents(NDataTable).map((table) => table.props('scrollX'))
    expect(scrollXs).toEqual([680, 760])
    // 两个 GET 都发出。
    const urls = fetchMock.mock.calls.map((c) => String(c[0]))
    expect(urls.some((u) => u === '/api/v1/agents')).toBe(true)
    expect(urls.some((u) => u === '/api/v1/upgrade-packages')).toBe(true)
  })

  it('排空/升级阶段列取真值：draining → 是，upgrade_phase=downloading → NProgress 50% + 下载中', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, draining: true, upgrade_phase: 'downloading' })]),
    )
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // 状态列 NTag；排空列 = 是；阶段列 = NProgress(50%) + 阶段文案。
    const cells = w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')[0]!.findAll('td')
    // 列序：Agent / 状态 / 版本 / 排空 / 升级阶段
    expect(cells[3]!.text()).toBe('是')
    expect(cells[4]!.text()).toContain('下载中')
    const progress = w.findComponent(NProgress)
    expect(progress.props('percentage')).toBe(50)
    expect(progress.props('status')).toBe('default')
    // 状态徽标沿用列表页同源色标（NTag）：在线 + 排空 → 派生为「排空」黄标
    //（agentBadgeState 优先级 draining > online，ADR-0017）。
    expect(w.findComponent(NTag).props('type')).toBe('warning')
    expect(w.findComponent(NTag).text()).toBe('排空')
  })

  it('fallback 阶段 → NProgress error 红条（已退回旧版本）', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, upgrade_phase: 'fallback' })]),
    )
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    expect(w.text()).toContain('已退回旧版本')
    const progress = w.findComponent(NProgress)
    expect(progress.props('percentage')).toBe(100)
    expect(progress.props('status')).toBe('error')
  })

  it('包上传：NUpload 选文件即传 → POST /upgrade-packages（X-Sisyphus-Filename 头 + body）', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1')]))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/upgrade-packages',
      jsonResponse(201, pkg('sisyphus-agent-1.0.0-linux-x86_64.tar.gz')),
    )
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // NUpload 内部 input[type=file]：注入 files + 派发 change 即触发 custom-request。
    const input = w.get('input[type="file"]').element as HTMLInputElement
    const file = new File(['x'], 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz', { type: 'application/octet-stream' })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await input.dispatchEvent(new Event('change'))

    // 成功 toast（NMessage teleport 到 body）。
    await vi.waitFor(() => expect(document.body.textContent).toContain('已上传'))

    const uploadCall = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/upgrade-packages' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(uploadCall).toBeTruthy()
    expect(uploadCall![1]!.headers).toMatchObject({ 'X-Sisyphus-Filename': 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz' })
    expect(uploadCall![1]!.body).toBe(file)
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
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-packages-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    await w.get('button[name="upgrade-all"]').trigger('click')
    await vi.waitFor(() => expect(document.body.textContent).toContain('已下发升级指令'))

    const call = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/agents/upgrade' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(call).toBeTruthy()
    expect(JSON.parse(call![1]!.body as string)).toEqual({ package_name: 'sisyphus-agent-1.0.0-linux-x86_64.tar.gz' })
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
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.upgrade-agents-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    await w.get('button[name="upgrade-one"]').trigger('click')
    await vi.waitFor(() => expect(document.body.textContent).toContain('已向 linux-1 下发升级指令'))

    expect(
      fetchMock.mock.calls.some(
        (c) => String(c[0]) === '/api/v1/agents/linux-1/upgrade' && (c[1]?.method ?? 'POST') === 'POST',
      ),
    ).toBe(true)
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染动作/表格', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('仅全局管理员可见'))
    expect(w.find('.n-card').exists()).toBe(false)
    expect(w.find('.n-data-table').exists()).toBe(false)
  })

  it('无 Agent → 空态提示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    setRoute('GET', '/api/v1/upgrade-packages', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.n-empty').exists()).toBe(true))
    expect(w.text()).toContain('暂无构建机')
  })
})

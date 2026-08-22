// Agent 详情行为测试（ADR-0008/0011/0012/0019，票 B4-T5/B5-T4）：只测外部行为，
// API 层以 fetch mock 驱动。详情页是「看」的表面：标签分区（NDescriptions）、
// 槽位占用 + 磁盘三口径（NStatistic/NDataTable）、工作区/缓存清理
// （B5-T4 起经通道往返真实下发；清理/删除改 NPopconfirm 确认——弹层
// teleport 到 body，断言经 document.querySelector 定位，与 BuildDetailView.spec
// 同纪律）。视图在 onMounted 即发详情请求：mount 须在设置 fetch mock 之后。

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
    agent_version: { major: 1, minor: 0, patch: 0 },
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

/** NPopconfirm 确认（positive 按钮 = action 末位；首按钮为取消）。 */
async function confirmPopconfirm(): Promise<void> {
  await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
  const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
  await (actionButtons[actionButtons.length - 1] as HTMLElement).click()
}

describe('AgentDetailView 详情（标签 + 槽位 + 磁盘 + 工作区/缓存清理）', () => {
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
    wrapper = mount(AgentDetailView, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
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
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  /** 详情体渲染完成信号（徽标出现即数据已到）。 */
  async function waitForAgent(): Promise<void> {
    await vi.waitFor(() => expect(wrapper!.find('.agent-state-tag').exists()).toBe(true))
  }

  it('加载详情：描述列表（标签分区 + 心跳/版本）+ 槽位 NStatistic + 返回链接', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    mountView()
    await waitForAgent()

    // 标签分区在 NDescriptions 内（NTag chips）。
    expect(wrapper!.find('.n-descriptions').exists()).toBe(true)
    expect(wrapper!.text()).toContain('sisyphus/os=linux')
    expect(wrapper!.text()).toContain('region=cn')
    // 槽位数值走 NStatistic。
    expect(wrapper!.findAll('.n-statistic').length).toBeGreaterThanOrEqual(4)
    expect(wrapper!.text()).toContain('在途任务')
    expect(wrapper!.text()).toContain('并发槽位')
    // Agent 版本（1.0.0）。
    expect(wrapper!.text()).toContain('1.0.0')
    expect(wrapper!.get('.agent-back').text()).toContain('返回 Agent 列表')
    expect(String(fetchMock.mock.calls[0]![0])).toBe('/api/v1/agents/demo')
  })

  it('磁盘三口径：卷级 total/free（NDataTable）+ 缓存占用 + 工作区占用（NStatistic/formatBytes）', async () => {
    setRoute(
      'GET',
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
    mountView()
    await waitForAgent()
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.agent-volume-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(2),
    )
    const rows = wrapper!.findAll('.agent-volume-table .n-data-table-tbody .n-data-table-tr')
    expect(rows[0]!.text()).toContain('/')
    expect(rows[0]!.text()).toContain('97.7 KB') // total 100_000
    expect(rows[0]!.text()).toContain('39.1 KB') // free 40_000
    expect(rows[1]!.text()).toContain('D:')
    expect(wrapper!.text()).toContain('缓存占用')
    expect(wrapper!.text()).toContain('4.9 KB') // cache 5_000
    expect(wrapper!.text()).toContain('工作区占用')
    expect(wrapper!.text()).toContain('7.8 KB') // workspace 8_000
  })

  it('disk_usage 为空 → 从未上报磁盘占用', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(200, agent('demo', { disk_usage: null })))
    mountView()
    await waitForAgent()
    expect(wrapper!.text()).toContain('从未上报磁盘占用')
    expect(wrapper!.find('.agent-volume-table').exists()).toBe(false)
  })

  it('工作区列表：查询 → POST /agents/demo/workspace/list → 表格', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    setRoute(
      'POST',
      '/api/v1/agents/demo/workspace/list',
      jsonResponse(200, {
        entries: [
          { pipeline: 'demo', job: 'compile', path: '/ws/demo/compile', last_used_at_ms: 1_700_000_000_000 },
        ],
      }),
    )
    mountView()
    await waitForAgent()

    await wrapper!.get('button[name="ws-list"]').trigger('click')
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.cleanup-card .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )
    expect(wrapper!.findAll('.cleanup-card .n-data-table-tbody .n-data-table-tr')[0]!.text()).toContain('/ws/demo/compile')
    expect(
      fetchMock.mock.calls.some(
        (c) => String(c[0]) === '/api/v1/agents/demo/workspace/list' && (c[1]?.method ?? 'POST') === 'POST',
      ),
    ).toBe(true)
  })

  it('工作区清理（NPopconfirm 确认）：填 pipeline/job → POST /agents/demo/workspace/clean', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    setRoute('POST', '/api/v1/agents/demo/workspace/clean', jsonResponse(202, ''))
    setRoute('POST', '/api/v1/agents/demo/workspace/list', jsonResponse(200, { entries: [] }))
    mountView()
    await waitForAgent()

    const wsPipeline = wrapper!.get('input[name="ws-pipeline"]')
    await wsPipeline.setValue('demo')
    await wrapper!.get('input[name="ws-job"]').setValue('compile')
    // 危险操作：点「清理」先弹确认层，确认后才下发。
    await wrapper!.get('button[name="ws-clean"]').trigger('click')
    expect(
      fetchMock.mock.calls.some((c) => String(c[0]) === '/api/v1/agents/demo/workspace/clean'),
    ).toBe(false)
    await confirmPopconfirm()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('工作区清理指令已下发'))

    const call = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/agents/demo/workspace/clean' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(call).toBeTruthy()
    expect(JSON.parse(call![1]!.body as string)).toEqual({ pipeline: 'demo', job: 'compile' })
  })

  it('缓存列表 + 删除（NPopconfirm 确认）：经通道往返', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(200, agent('demo')))
    setRoute(
      'POST',
      '/api/v1/agents/demo/cache/list',
      jsonResponse(200, {
        entries: [{ key: 'cargo-abc', pipeline: 'demo', size_bytes: 5_000, last_used_at_ms: 1_700_000_000_000 }],
      }),
    )
    setRoute('POST', '/api/v1/agents/demo/cache/delete', jsonResponse(202, ''))
    mountView()
    await waitForAgent()

    await wrapper!.get('button[name="cache-list"]').trigger('click')
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.cleanup-card .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )
    expect(wrapper!.findAll('.cleanup-card .n-data-table-tbody .n-data-table-tr')[0]!.text()).toContain('cargo-abc')

    await wrapper!.get('input[name="cache-key"]').setValue('cargo-abc')
    await wrapper!.get('button[name="cache-delete"]').trigger('click')
    expect(
      fetchMock.mock.calls.some((c) => String(c[0]) === '/api/v1/agents/demo/cache/delete'),
    ).toBe(false)
    await confirmPopconfirm()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('缓存删除指令已下发'))
    const call = fetchMock.mock.calls.find(
      (c) => String(c[0]) === '/api/v1/agents/demo/cache/delete' && (c[1]?.method ?? 'POST') === 'POST',
    )
    expect(JSON.parse(call![1]!.body as string)).toEqual({ key: 'cargo-abc' })
  })

  it('标签为空 → （无）占位', async () => {
    setRoute(
      'GET',
      '/api/v1/agents/demo',
      jsonResponse(200, agent('demo', { system_labels: [], custom_labels: [] })),
    )
    mountView()
    await waitForAgent()
    expect(wrapper!.findAll('.form-hint').some((e) => e.text() === '（无）')).toBe(true)
  })

  it('404 → Agent 不存在', async () => {
    setRoute(
      'GET',
      '/api/v1/agents/demo',
      jsonResponse(404, { code: 'NOT_FOUND', message: 'Agent demo 不存在' }),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('Agent 不存在'))
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染详情体', async () => {
    setRoute('GET', '/api/v1/agents/demo', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('仅全局管理员可见'))
    expect(wrapper!.find('.agent-state-tag').exists()).toBe(false)
    expect(wrapper!.find('.n-descriptions').exists()).toBe(false)
  })

  it('停用 Agent 详情：徽标展示「停用」', async () => {
    setRoute(
      'GET',
      '/api/v1/agents/demo',
      jsonResponse(200, agent('demo', { online: true, disabled: true })),
    )
    mountView()
    await waitForAgent()
    const badge = wrapper!.get('.agent-state-tag')
    expect(badge.text()).toBe('停用')
  })
})

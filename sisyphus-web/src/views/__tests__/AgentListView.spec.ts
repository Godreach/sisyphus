// 构建机页行为测试（原型页三重构，spec #99；数据面 = ADR-0008/0010/0017）。
// 只测外部行为，API 层以 fetch mock（method + URL 前缀路由）驱动。
// 视图在 onMounted 即发列表请求：mount 须在设置 fetch mock 之后。
// - 四张指标卡：总数/在线/离线/异常（停用计数进总数副标）
// - 状态徽章（原型胶囊形态）：构建中/在线/离线/停用/排空/版本不兼容
// - 槽位/磁盘进度条（原型 usage-cell）
// - 建条目（空态按钮）：POST /agents → 一次性 token + 注册码 + 刷新
// - 停用/启用：NSwitch → PATCH { disabled }；编辑弹窗 → PATCH 槽位+标签
// - 403（非全局 admin）→ admin-only 退化态；409 重名；422 校验
// NModal teleport 不在 wrapper 内——弹窗断言经 document.querySelector 定位。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import AgentListView from '@/views/AgentListView.vue'
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

/** 包装组件：NMessageProvider + AgentListView，保证 useMessage 注入可用。 */
const Host = defineComponent({
  name: 'AgentListHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AgentListView, { ...attrs }))
  },
})

describe('AgentListView 构建机页（指标卡 + 资源表 + 动作流）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** method + URL 前缀 → 响应（最长前缀匹配，按 method 分流 GET/POST/PATCH）。 */
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
    wrapper = mount(Host, {
      global: { plugins: [pinia, router, i18n] },
    })
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
        { path: '/machines', name: 'machines', component: { template: '<div />' } },
        { path: '/agents/:name', name: 'agent-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/machines')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  /** 在已打开的 NModal 内按 name 定位输入并注入输入事件（teleport 到 body）。 */
  async function setModalInput(selector: string, value: string): Promise<void> {
    const el = document.querySelector(selector) as HTMLInputElement | HTMLTextAreaElement
    if (!el) throw new Error(`modal input not found: ${selector}`)
    el.value = value
    await el.dispatchEvent(new Event('input'))
  }

  it('加载后列出构建机 + 展示态徽章（构建中/在线/离线/停用/排空/版本不兼容）', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [
        agent('linux-1', { online: true, active_jobs: 1 }),
        agent('linux-2', { online: false }),
        agent('win-1', { online: true, disabled: true }),
        agent('mac-1', { online: true, draining: true }),
        agent('old-1', { online: true, version_compatible: false }),
      ]),
    )
    mountView()
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(5),
    )

    // 状态列（原型胶囊徽章）：文案 + 色类。
    const badges = wrapper!.findAll('.n-data-table-tbody .n-data-table-tr .badge')
    expect(badges.map((b) => b.text())).toEqual([
      '构建中',
      '离线',
      '停用',
      '排空',
      '版本不兼容',
    ])
    expect(badges.map((b) => b.classes().join(' '))).toEqual([
      expect.stringContaining('building'),
      expect.stringContaining('offline'),
      expect.stringContaining('neutral'),
      expect.stringContaining('draining'),
      expect.stringContaining('failed'),
    ])
  })

  it('四张指标卡：总数（停用副标）/在线（可用率）/离线/异常', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [
        agent('linux-1', { online: true }),
        agent('linux-2', { online: false }),
        agent('win-1', { online: true, disabled: true }),
        agent('mac-1', { online: true, draining: true }),
        agent('old-1', { online: true, version_compatible: false }),
      ]),
    )
    mountView()
    // 等真实指标卡（骨架屏里也有 .metric-row 壳，须以卡片数为准）。
    await vi.waitFor(() => expect(wrapper!.findAll('.metric-card')).toHaveLength(4))

    const cards = wrapper!.findAll('.metric-card')
    expect(cards).toHaveLength(4)
    // 总数 5，副标 停用 1 台。
    expect(cards[0]!.text()).toContain('5')
    expect(cards[0]!.text()).toContain('停用 1 台')
    // 在线 = 启用且 online = 3（linux-1/mac-1/old-1），可用率 75%。
    expect(cards[1]!.text()).toContain('3')
    expect(cards[1]!.text()).toContain('75% 可用')
    // 离线 = 启用但非 online = 1。
    expect(cards[2]!.text()).toContain('1')
    // 异常 = 排空 + 不兼容 = 2。
    expect(cards[3]!.text()).toContain('2')
  })

  it('槽位/磁盘列渲染原型进度条（fill 宽度 + 副标）', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [
        agent('linux-1', {
          online: true,
          active_jobs: 1,
          max_concurrency: 2,
          disk_usage: {
            volumes: [{ mount_point: '/', total_bytes: 1_000_000_000, free_bytes: 500_000_000 }],
            cache_bytes: 0,
            workspace_bytes: 0,
          },
        }),
      ]),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.usage-cell').exists()).toBe(true))

    const cells = wrapper!.findAll('.usage-cell')
    expect(cells).toHaveLength(2) // 槽位 + 磁盘
    // 槽位 1/2 → 50%。
    expect(cells[0]!.find('.pct').text()).toBe('50%')
    expect(cells[0]!.find('.fill').attributes('style')).toContain('width: 50%')
    expect(cells[0]!.text()).toContain('1 / 2')
    // 磁盘 5e8/1e9 字节（1024 进制）→ 50%，476.8 MB / 953.7 MB。
    expect(cells[1]!.find('.pct').text()).toBe('50%')
    expect(cells[1]!.text()).toContain('476.8 MB')
    expect(cells[1]!.text()).toContain('953.7 MB')
  })

  it('无 Agent：NEmpty 空态 + 注册引导 + 接入入口', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.n-empty').exists()).toBe(true))
    expect(wrapper!.text()).toContain('暂无构建机')
    expect(wrapper!.text()).toContain('在构建机上执行注册命令')
    expect(wrapper!.find('button[name="agent-new-empty"]').exists()).toBe(true)
    expect(wrapper!.find('.n-data-table').exists()).toBe(false)
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染指标卡/列表', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('仅全局管理员可见'))
    expect(wrapper!.find('.n-data-table').exists()).toBe(false)
    expect(wrapper!.find('.metric-row').exists()).toBe(false)
  })

  it('建条目（空态接入按钮）：POST /agents → 一次性 token + 注册码 + 刷新列表', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.n-empty').exists()).toBe(true))

    // 刷新后列表含新 Agent。
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: false })]),
    )
    setRoute(
      'POST',
      '/api/v1/agents',
      jsonResponse(201, {
        token: 'sisa_T0K3N',
        register_code: 'sisa_reg_C0D3',
        agent: agent('linux-1'),
      }),
    )

    await wrapper!.get('button[name="agent-new-empty"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="agent-name"]', 'linux-1')
    await setModalInput('.n-modal textarea[name="agent-labels"]', 'region=cn')
    await setModalInput('.n-modal input[name="agent-concurrency"]', '2')
    await (document.querySelector('.n-modal button[name="agent-create"]') as HTMLElement).click()

    // 凭据弹窗：token + 注册码明文仅此一次 + 按 OS 复制命令（--reg-key 换码）。
    let credsModal: Element | undefined
    await vi.waitFor(() => {
      credsModal = [...document.querySelectorAll('.n-modal')].find((m) =>
        m.textContent?.includes('sisa_T0K3N'),
      )
      expect(credsModal).toBeTruthy()
    })
    const modalText = credsModal!.textContent ?? ''
    expect(modalText).toContain('sisa_reg_C0D3')
    expect(modalText).toContain('--reg-key sisa_reg_C0D3')

    // 提交形态：POST /api/v1/agents，标签解析为数组、槽位为数值。
    const post = fetchMock.mock.calls.find(
      (c) => (c[1] as RequestInit | undefined)?.method === 'POST',
    ) as [string, RequestInit]
    expect(post[0]).toBe('/api/v1/agents')
    expect(JSON.parse(post[1].body as string)).toEqual({
      name: 'linux-1',
      custom_labels: ['region=cn'],
      max_concurrency: 2,
    })

    // 成功即刷新列表（再次 GET /agents）——新 Agent 入列。
    await vi.waitFor(() => expect(wrapper!.text()).toContain('linux-1'))
  })

  it('建条目 409（重名）→ 弹窗内展示错误，表单停留', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.n-empty').exists()).toBe(true))

    setRoute(
      'POST',
      '/api/v1/agents',
      jsonResponse(409, { code: 'CONFLICT', message: 'Agent 名已存在：linux-1' }),
    )
    await wrapper!.get('button[name="agent-new-empty"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="agent-name"]', 'linux-1')
    await (document.querySelector('.n-modal button[name="agent-create"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('Agent 名已存在'),
    )
    // 表单未收（弹窗停留，自定义标签仍在）。
    expect(document.querySelector('.n-modal')?.textContent).toContain('自定义标签')
  })

  it('建条目 422（标签形态脏）→ 拼接校验清单就地展示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.n-empty').exists()).toBe(true))

    setRoute(
      'POST',
      '/api/v1/agents',
      jsonResponse(422, {
        code: 'VALIDATION_FAILED',
        message: 'Agent 输入校验失败',
        detail: {
          errors: [
            { path: 'custom_labels', message: '标签须为 key=value 形态且键/值非空："region"' },
          ],
        },
      }),
    )
    await wrapper!.get('button[name="agent-new-empty"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="agent-name"]', 'linux-1')
    await setModalInput('.n-modal textarea[name="agent-labels"]', 'region')
    await (document.querySelector('.n-modal button[name="agent-create"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('key=value 形态'),
    )
  })

  it('停用/启用：NSwitch 切换 → PATCH /agents/{name} { disabled } + 刷新', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, disabled: false })]),
    )
    setRoute(
      'PATCH',
      '/api/v1/agents/linux-1',
      jsonResponse(200, agent('linux-1', { online: true, disabled: true })),
    )
    mountView()
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // 刷新后列表回读：停用 Agent 回到列表（disabled=true）。
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, disabled: true })]),
    )

    // 初始开关 = 启用态（active）。
    const sw = wrapper!.get('.machine-toggle')
    expect(sw.classes()).toContain('n-switch--active')
    await sw.trigger('click')

    // PATCH 形态：{ disabled: true }。
    await vi.waitFor(() => {
      const patch = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PATCH',
      ) as [string, RequestInit]
      expect(patch[0]).toBe('/api/v1/agents/linux-1')
      expect(JSON.parse(patch[1].body as string)).toEqual({ disabled: true })
    })
    // 刷新后开关翻为停用态（非 active）。
    await vi.waitFor(() =>
      expect(wrapper!.get('.machine-toggle').classes()).not.toContain('n-switch--active'),
    )
  })

  it('编辑槽位/自定义标签：弹窗预填 → PATCH { max_concurrency, custom_labels }（整组替换）', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { max_concurrency: 1, custom_labels: ['region=cn'] })]),
    )
    setRoute(
      'PATCH',
      '/api/v1/agents/linux-1',
      jsonResponse(200, agent('linux-1', { max_concurrency: 3, custom_labels: ['region=eu', 'gpu=nvidia'] })),
    )
    mountView()
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // 行内「编辑」按钮（动作列；NSwitch 非 button 元素，首个 button 即编辑）。
    await wrapper!.get('.machine-row-actions button').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')?.textContent).toContain('编辑构建机'))
    expect((document.querySelector('.n-modal input[name="edit-concurrency"]') as HTMLInputElement)?.value).toBe('1')
    expect((document.querySelector('.n-modal textarea[name="edit-labels"]') as HTMLTextAreaElement)?.value).toBe('region=cn')

    await setModalInput('.n-modal input[name="edit-concurrency"]', '3')
    await setModalInput('.n-modal textarea[name="edit-labels"]', 'region=eu\ngpu=nvidia')
    const saveBtn = [...document.querySelectorAll('.n-modal button')].find(
      (b) => b.textContent?.trim() === '保存',
    )
    await (saveBtn as HTMLElement).click()

    // PATCH 形态：槽位 + 标签整组替换（每行一条 → 数组）。
    await vi.waitFor(() => {
      const patch = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PATCH',
      ) as [string, RequestInit]
      expect(patch[0]).toBe('/api/v1/agents/linux-1')
      expect(JSON.parse(patch[1].body as string)).toEqual({
        max_concurrency: 3,
        custom_labels: ['region=eu', 'gpu=nvidia'],
      })
    })
  })

  it('点击构建机名 → 跳详情页；详情按钮同效', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('linux-1'))

    const pushSpy = vi.spyOn(router, 'push')
    await wrapper!.get('.machine-name-btn').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'agent-detail',
      params: { name: 'linux-1' },
    })
  })
})

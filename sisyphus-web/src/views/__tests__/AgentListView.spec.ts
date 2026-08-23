// Agent 列表行为测试（ADR-0008/0010/0017，票 B4-T5）：只测外部行为，API 层
// 以 fetch mock（method + URL 前缀路由）驱动。视图在 onMounted 即发列表请求：
// mount 须在设置 fetch mock 之后。
// - 列表四态徽标（在线/离线/停用；排空/不兼容 退化标注）：NTag 颜色编码
//   经组件 props 断言（type 影响的是内联 CSS 变量，无 type 类可查）
// - 建条目：POST /agents → 一次性 token + 注册码 + 按 OS 复制命令 + 刷新列表
// - 停用/启用：NSwitch 切换 → PATCH /agents/{name} { disabled }
// - 编辑槽位/自定义标签：编辑弹窗 → PATCH { max_concurrency, custom_labels }
// - 403（非全局 admin）→ admin-only 退化态；409 重名；422 校验
// #94: NModal/NDataTable teleport 不在 wrapper 内——弹窗断言经
// document.querySelector 定位（与 BuildDetailView.spec 同纪律）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NDataTable, NMessageProvider, NTag } from 'naive-ui'
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
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

/** 包装组件：NMessageProvider + AgentListView，保证 useMessage 注入可用。 */
const AgentListWrapper = defineComponent({
  name: 'AgentListWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AgentListView, { ...attrs }))
  },
})

describe('AgentListView 列表 + 建条目 + 停用/启用 + 编辑', () => {
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
    wrapper = mount(AgentListWrapper, {
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
        { path: '/agents', name: 'agents', component: { template: '<div />' } },
        { path: '/agents/:name', name: 'agent-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/agents')
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

  it('加载后列出 Agent + 四态徽标（在线/离线/停用/排空/不兼容，B5-T4 起全派生）', async () => {
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
    await vi.waitFor(() =>
      expect(wrapper!.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(5),
    )

    // 状态列 NTag：文案 + 颜色编码（type → success/error/warning/default）。
    const tags = wrapper!.findAllComponents(NTag)
    const texts = tags.map((tag) => tag.text())
    const types = tags.map((tag) => tag.props('type'))
    expect(texts).toEqual(['在线', '离线', '停用', '排空', '版本不兼容'])
    expect(types).toEqual(['success', 'error', 'default', 'warning', 'default'])

    // 平板窄视口：Agent 表设最小表宽，容器更窄时横向滚动而非挤压列。
    expect(wrapper!.findComponent(NDataTable).props('scrollX')).toBe(700)
  })

  it('无 Agent：NEmpty 空态 + 注册引导 + 新建入口', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('.n-empty').exists()).toBe(true))
    expect(wrapper!.text()).toContain('暂无 Agent')
    expect(wrapper!.text()).toContain('在构建机上执行注册命令')
    expect(wrapper!.find('button[name="agent-new-empty"]').exists()).toBe(true)
    expect(wrapper!.find('.n-data-table').exists()).toBe(false)
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染列表/动作', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('仅全局管理员可见'))
    expect(wrapper!.find('.n-data-table').exists()).toBe(false)
    expect(wrapper!.find('button[name="agent-new"]').exists()).toBe(false)
  })

  it('建条目：POST /agents → 一次性 token + 注册码 + 按 OS 复制命令 + 刷新列表', async () => {
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

    await wrapper!.get('button[name="agent-new"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="agent-name"]', 'linux-1')
    await setModalInput('.n-modal textarea[name="agent-labels"]', 'region=cn')
    await setModalInput('.n-modal input[name="agent-concurrency"]', '2')
    await (document.querySelector('.n-modal button[name="agent-create"]') as HTMLElement).click()

    // 凭据弹窗：token + 注册码明文仅此一次 + 按 OS 复制命令（--reg-key 换码）。
    // 表单弹窗关闭动画期间两者并存——按内容定位凭据弹窗。
    let credsModal: Element | undefined
    await vi.waitFor(() => {
      credsModal = [...document.querySelectorAll('.n-modal')].find((m) =>
        m.textContent?.includes('sisa_T0K3N'),
      )
      expect(credsModal).toBeTruthy()
    })
    const modalText = credsModal!.textContent ?? ''
    expect(modalText).toContain('sisa_reg_C0D3')
    expect(modalText).toContain('sisyphus-agent')
    expect(modalText).toContain('--reg-key sisa_reg_C0D3')
    expect(modalText).not.toContain('.exe') // 默认 Linux/macOS 档

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
    await wrapper!.get('button[name="agent-new"]').trigger('click')
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
    await wrapper!.get('button[name="agent-new"]').trigger('click')
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

    // 切换后列表回读：停用 Agent 回到列表（disabled=true）。
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, disabled: true })]),
    )

    // 初始开关 = 启用态（active）。
    const sw = wrapper!.get('.agent-toggle')
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
      expect(wrapper!.get('.agent-toggle').classes()).not.toContain('n-switch--active'),
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

    // 行内「编辑」按钮（动作列）→ 打开编辑弹窗（预填当前值）。
    await wrapper!.get('.agent-row-actions button').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')?.textContent).toContain('编辑 Agent'))
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

  it('点击 Agent 名 → 跳详情页', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('linux-1'))

    const pushSpy = vi.spyOn(router, 'push')
    await wrapper!.get('.agent-name-btn').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'agent-detail',
      params: { name: 'linux-1' },
    })
  })
})

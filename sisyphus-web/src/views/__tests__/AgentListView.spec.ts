// Agent 列表行为测试（ADR-0008/0010/0017，票 B4-T5）：只测外部行为，API 层
// 以 fetch mock（method + URL 前缀路由）驱动。视图在 onMounted 即发列表请求：
// mount 须在设置 fetch mock 之后。
// - 列表四态徽标（在线/离线/停用；排空/不兼容 退化标注）
// - 建条目：POST /agents → 一次性 token + 注册码 + 按 OS 复制命令 + 刷新列表
// - 停用/启用：PATCH /agents/{name} { disabled }
// - 编辑槽位/自定义标签：PATCH { max_concurrency, custom_labels }
// - 403（非全局 admin）→ admin-only 退化态；409 重名；422 校验

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

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

describe('AgentListView 列表 + 建条目 + 停用/启用 + 编辑', () => {
  let pinia: Pinia
  let router: Router

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
    return mount(AgentListView, {
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
    await router.push('/agents')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

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
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.agent-item').length).toBe(5))

    const badges = wrapper.findAll('.agent-state-badge')
    expect(badges[0]!.text()).toBe('在线')
    expect(badges[0]!.classes()).toContain('agent-state-online')
    expect(badges[1]!.text()).toBe('离线')
    expect(badges[1]!.classes()).toContain('agent-state-offline')
    // 停用优先（停用即踢线，online=true 仍展示「停用」不展示「在线」）。
    expect(badges[2]!.text()).toBe('停用')
    expect(badges[2]!.classes()).toContain('agent-state-disabled')
    // 排空（在线 + draining）→ 排空徽标。
    expect(badges[3]!.text()).toBe('排空')
    expect(badges[3]!.classes()).toContain('agent-state-draining')
    // 版本不兼容（version_compatible=false，即便在线）→ 不兼容徽标。
    expect(badges[4]!.text()).toBe('版本不兼容')
    expect(badges[4]!.classes()).toContain('agent-state-incompatible')
    wrapper.unmount()
  })

  it('无 Agent：展示空态提示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))
    wrapper.unmount()
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染列表/动作', async () => {
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    expect(wrapper.find('.agent-list').exists()).toBe(false)
    expect(wrapper.find('button[name="agent-new"]').exists()).toBe(false)
    wrapper.unmount()
  })

  it('建条目：POST /agents → 一次性 token + 注册码 + 按 OS 复制命令 + 刷新列表', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))

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

    await wrapper.get('button[name="agent-new"]').trigger('click')
    await wrapper.get('input[name="agent-name"]').setValue('linux-1')
    await wrapper.get('textarea[name="agent-labels"]').setValue('region=cn')
    await wrapper.get('input[name="agent-concurrency"]').setValue('2')
    await wrapper.get('button[name="agent-create"]').trigger('click')

    // 凭据面板：token + 注册码明文仅此一次 + 按 OS 复制命令（--reg-key 换码）。
    await vi.waitFor(() => expect(wrapper.text()).toContain('sisa_T0K3N'))
    expect(wrapper.text()).toContain('sisa_reg_C0D3')
    const cmd = wrapper.get('.agent-cmd code').text()
    expect(cmd).toContain('sisyphus-agent')
    expect(cmd).toContain('--reg-key sisa_reg_C0D3')
    expect(cmd).not.toContain('.exe') // 默认 Linux/macOS 档

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
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))
    wrapper.unmount()
  })

  it('建条目 409（重名）→ 就地展示错误，表单停留', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))

    setRoute(
      'POST',
      '/api/v1/agents',
      jsonResponse(409, { code: 'CONFLICT', message: 'Agent 名已存在：linux-1' }),
    )
    await wrapper.get('button[name="agent-new"]').trigger('click')
    await wrapper.get('input[name="agent-name"]').setValue('linux-1')
    await wrapper.get('button[name="agent-create"]').trigger('click')

    await vi.waitFor(() =>
      expect(wrapper.get('[role="alert"]').text()).toContain('Agent 名已存在'),
    )
    expect(wrapper.text()).toContain('自定义标签') // 表单未收（停留）
    wrapper.unmount()
  })

  it('建条目 422（标签形态脏）→ 拼接校验清单就地展示', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无 Agent'))

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
    await wrapper.get('button[name="agent-new"]').trigger('click')
    await wrapper.get('input[name="agent-name"]').setValue('linux-1')
    await wrapper.get('textarea[name="agent-labels"]').setValue('region')
    await wrapper.get('button[name="agent-create"]').trigger('click')

    await vi.waitFor(() =>
      expect(wrapper.get('[role="alert"]').text()).toContain('key=value 形态'),
    )
    wrapper.unmount()
  })

  it('停用/启用：PATCH /agents/{name} { disabled } + 刷新', async () => {
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
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.agent-item').length).toBe(1))

    // 切换后列表回读：停用 Agent 回到列表（disabled=true）。
    setRoute(
      'GET',
      '/api/v1/agents',
      jsonResponse(200, [agent('linux-1', { online: true, disabled: true })]),
    )

    // 初始按钮为「停用」（disabled=false）。
    expect(wrapper.get('.agent-toggle').text()).toBe('停用')
    await wrapper.get('.agent-toggle').trigger('click')

    // PATCH 形态：{ disabled: true }。
    await vi.waitFor(() => {
      const patch = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PATCH',
      ) as [string, RequestInit]
      expect(patch[0]).toBe('/api/v1/agents/linux-1')
      expect(JSON.parse(patch[1].body as string)).toEqual({ disabled: true })
    })
    // 刷新后按钮翻为「启用」（disabled=true）。
    await vi.waitFor(() => expect(wrapper.get('.agent-toggle').text()).toBe('启用'))
    wrapper.unmount()
  })

  it('编辑槽位/自定义标签：PATCH { max_concurrency, custom_labels }（整组替换）', async () => {
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
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.agent-item').length).toBe(1))

    // 展开编辑表单（预填当前值）。
    await wrapper.get('.agent-edit').trigger('click')
    expect((wrapper.get('input[name="edit-concurrency"]').element as HTMLInputElement).value).toBe('1')
    expect((wrapper.get('textarea[name="edit-labels"]').element as HTMLTextAreaElement).value).toBe('region=cn')

    await wrapper.get('input[name="edit-concurrency"]').setValue('3')
    await wrapper.get('textarea[name="edit-labels"]').setValue('region=eu\ngpu=nvidia')
    await wrapper.get('.agent-save').trigger('click')

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
    wrapper.unmount()
  })

  it('点击 Agent 名 → 跳详情页', async () => {
    setRoute('GET', '/api/v1/agents', jsonResponse(200, [agent('linux-1', { online: true })]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('linux-1'))

    const pushSpy = vi.spyOn(router, 'push')
    await wrapper.get('.agent-name-btn').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'agent-detail',
      params: { name: 'linux-1' },
    })
    wrapper.unmount()
  })
})

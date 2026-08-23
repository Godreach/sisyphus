// 用户 / PAT 页行为测试（ADR-0014，票 B4-T6）：只测外部行为，API 层以 fetch
// mock（method + URL 前缀路由，最长前缀匹配）驱动。视图在 onMounted 即并发
// 加载用户与 PAT：mount 须在设置 fetch mock 之后。
// - 用户清单 + 角色/状态 NTag；启用/禁用 NSwitch
// - 建号：POST /users { username, password, is_admin }（建号时设全局 admin）+ 刷新
// - 禁用/启用：NSwitch → PATCH /users/{name} { disabled }；
//   重置密码：弹窗 → PUT /users/{name}/password
// - PAT：创建 POST /auth/tokens → 一次性明文令牌（NCode）+ 刷新；
//   吊销 NPopconfirm → DELETE /auth/tokens/{id}
// - 403 → admin-only 退化态
// #95: NModal/NPopconfirm teleport 不在 wrapper 内——弹窗断言经
// document 定位（与 AgentListView.spec 同纪律）；NMessage toast 文案经
// document.body 断言。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NDataTable, NMessageProvider, NTag } from 'naive-ui'
import { defineComponent, h } from 'vue'

import UsersView from '@/views/UsersView.vue'
import { i18n, setLocale } from '@/i18n'
import type { UserResponse } from '@/api/types'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

function noContent(): Response {
  return new Response(null, { status: 204 })
}

function user(name: string, overrides: Partial<UserResponse> = {}): UserResponse {
  return {
    id: 1,
    username: name,
    is_admin: false,
    disabled: false,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

/** 包装组件：NMessageProvider + UsersView，保证 useMessage 注入可用。 */
const UsersWrapper = defineComponent({
  name: 'UsersWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(UsersView, { ...attrs }))
  },
})

describe('UsersView 用户生命周期 + PAT 一次明文', () => {
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
    wrapper = mount(UsersWrapper, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
  }

  /** 在已打开的 NModal 内按 name 定位输入并注入输入事件（teleport 到 body）。 */
  async function setModalInput(selector: string, value: string): Promise<void> {
    const el = document.querySelector(selector) as HTMLInputElement | null
    if (!el) throw new Error(`modal input not found: ${selector}`)
    el.value = value
    await el.dispatchEvent(new Event('input'))
  }

  /** 在已打开的 NPopconfirm 弹层内点击「确认」（teleport 到 body）。 */
  async function confirmPopconfirm(): Promise<void> {
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm')).toBeTruthy())
    const btn = [...document.querySelectorAll('.n-popconfirm button')].find(
      (b) => b.textContent?.trim() === '确认',
    )
    expect(btn, 'popconfirm 确认按钮').toBeTruthy()
    await (btn as HTMLElement).click()
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/users', name: 'admin-users', component: { template: '<div />' } }],
    })
    await router.push('/admin/users')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  it('加载用户清单 + 角色/状态 NTag（admin/普通、活跃/已禁用）', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute(
      'GET',
      '/api/v1/users',
      jsonResponse(200, [
        user('admin', { id: 1, is_admin: true }),
        user('alice', { id: 2 }),
        user('bob', { id: 3, disabled: true }),
      ]),
    )
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.users-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(3),
    )

    // 每行两枚 NTag：角色（admin=warning / 普通=default）+ 状态（活跃=success /
    // 已禁用=error）。
    const tags = w.findAllComponents(NTag)
    expect(tags.map((tag) => tag.text())).toEqual([
      '全局管理员', '活跃',
      '普通用户', '活跃',
      '普通用户', '已禁用',
    ])
    expect(tags.map((tag) => tag.props('type'))).toEqual([
      'warning', 'success',
      'default', 'success',
      'default', 'error',
    ])

    // 平板窄视口：用户表设最小表宽，容器更窄时横向滚动而非挤压列。
    expect(w.findComponent(NDataTable).props('scrollX')).toBe(560)
  })

  it('建号：POST /users { username, password, is_admin }（建号时设全局 admin）+ 刷新', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/users',
      jsonResponse(201, user('bob', { id: 9, is_admin: true })),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('暂无用户'))

    // 刷新后清单含新用户。
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('bob', { id: 9, is_admin: true })]))

    await w.get('button[name="user-new"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="user-username"]', 'bob')
    await setModalInput('.n-modal input[name="user-password"]', 'pw123456')
    // is_admin NSwitch 打开（默认关 → 点击翻为开）。
    await (document.querySelector('.n-modal .user-admin-switch') as HTMLElement).click()
    await (document.querySelector('.n-modal button[name="user-create"]') as HTMLElement).click()

    // 提交形态：POST /api/v1/users，is_admin 在建号时显式设。
    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'POST' && c[0] === '/api/v1/users',
      ) as [string, RequestInit]
      expect(post).toBeTruthy()
      expect(JSON.parse(post[1].body as string)).toEqual({
        username: 'bob',
        password: 'pw123456',
        is_admin: true,
      })
    })
    // 成功 toast + 刷新后新用户入列。
    await vi.waitFor(() => expect(document.body.textContent).toContain('用户已创建'))
    await vi.waitFor(() => expect(w.text()).toContain('bob'))
  })

  it('建号 409（重名）→ 弹窗内展示错误，表单停留', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/users',
      jsonResponse(409, { code: 'CONFLICT', message: '用户名已存在：bob' }),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('暂无用户'))

    await w.get('button[name="user-new"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="user-username"]', 'bob')
    await setModalInput('.n-modal input[name="user-password"]', 'pw123456')
    await (document.querySelector('.n-modal button[name="user-create"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('用户名已存在'),
    )
    // 表单未收（弹窗停留，全局 admin 开关仍在）。
    expect(document.querySelector('.n-modal .user-admin-switch')).toBeTruthy()
  })

  it('禁用/启用：NSwitch 切换 → PATCH /users/{name} { disabled } + 刷新后开关翻转', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice', { disabled: false })]))
    setRoute(
      'PATCH',
      '/api/v1/users/',
      jsonResponse(200, user('alice', { disabled: true })),
    )
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.users-table .n-data-table-tbody .n-data-table-tr')).toHaveLength(1),
    )

    // 初始开关 = 启用态（active）。
    const sw = w.get('.user-toggle')
    expect(sw.classes()).toContain('n-switch--active')

    // 刷新后回读为已禁用。
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice', { disabled: true })]))

    await sw.trigger('click')

    // PATCH 形态：{ disabled: true }。
    await vi.waitFor(() => {
      const patch = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PATCH',
      ) as [string, RequestInit]
      expect(patch[0]).toBe('/api/v1/users/alice')
      expect(JSON.parse(patch[1].body as string)).toEqual({ disabled: true })
    })
    // 成功 toast + 刷新后开关翻为禁用态（非 active）。
    await vi.waitFor(() => expect(document.body.textContent).toContain('用户已禁用'))
    await vi.waitFor(() =>
      expect(w.get('.user-toggle').classes()).not.toContain('n-switch--active'),
    )
  })

  it('重置密码：弹窗 → PUT /users/{name}/password { new_password }（204）+ 弹窗收起', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice')]))
    setRoute('PUT', '/api/v1/users/', noContent())
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('alice'))

    await w.get('button[name="user-reset"]').trigger('click')
    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('alice'),
    )
    await setModalInput('.n-modal input[name="reset-password"]', 'newpw123')
    await (document.querySelector('.n-modal button[name="reset-submit"]') as HTMLElement).click()

    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PUT',
      ) as [string, RequestInit]
      expect(put[0]).toBe('/api/v1/users/alice/password')
      expect(JSON.parse(put[1].body as string)).toEqual({ new_password: 'newpw123' })
    })
    // 成功 toast + 204 后弹窗收起。jsdom 中 NModal 关闭动画不完成、卡片 DOM
    // 滞留——但内容响应式清空（descriptions 用户名/输入值随 resettingUser
    // 置空），以弹窗不再展示该用户断言「收起」语义（与 PAT 一次性令牌丢弃
    // 同纪律）。
    await vi.waitFor(() => expect(document.body.textContent).toContain('密码已重置'))
    await vi.waitFor(() => {
      expect(document.querySelector('.n-modal')?.textContent ?? '').not.toContain('alice')
    })
  })

  it('PAT：创建 POST /auth/tokens → 一次性明文令牌（NCode）+ 刷新列表', async () => {
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/auth/tokens',
      jsonResponse(201, {
        token: 'sis_ABCDEFGHIJK_one_time',
        id: 7,
        name: 'ci-deploy',
        expires_at: null,
        created_at: 1_700_000_000_000,
      }),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('暂无令牌'))

    // 刷新后 PAT 列表含新令牌。
    setRoute(
      'GET',
      '/api/v1/auth/tokens',
      jsonResponse(200, [
        { id: 7, name: 'ci-deploy', expires_at: null, created_at: 1_700_000_000_000 },
      ]),
    )

    await w.get('button[name="token-new"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    await setModalInput('.n-modal input[name="token-name"]', 'ci-deploy')
    await (document.querySelector('.n-modal button[name="token-create"]') as HTMLElement).click()

    // 提交形态：POST /api/v1/auth/tokens，留空过期 = 不带 expires_at。
    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'POST' && c[0] === '/api/v1/auth/tokens',
      ) as [string, RequestInit]
      expect(JSON.parse(post[1].body as string)).toEqual({ name: 'ci-deploy' })
    })

    // 一次性明文令牌弹窗（NCode 明文仅此一次）+ 警示。表单弹窗关闭动画期间
    // 两者并存——按内容定位一次性令牌弹窗。
    let credsModal: Element | undefined
    await vi.waitFor(() => {
      credsModal = [...document.querySelectorAll('.n-modal')].find((m) =>
        m.textContent?.includes('sis_ABCDEFGHIJK_one_time'),
      )
      expect(credsModal).toBeTruthy()
    })
    expect(credsModal!.textContent).toContain('此后任何端点都无法找回')

    // 丢弃后令牌不再可见。
    await (credsModal!.querySelector('button[name="token-dismiss"]') as HTMLElement).click()
    await vi.waitFor(() =>
      expect(document.body.textContent).not.toContain('sis_ABCDEFGHIJK_one_time'),
    )
    // 刷新后列表含新令牌名。
    await vi.waitFor(() => expect(w.text()).toContain('ci-deploy'))
    // PAT 表同样设最小表宽（平板窄视口横向滚动，与用户表同纪律）。本用例
    // users=[] 走空态、PAT 表为唯一 NDataTable；按 DOM 序取末表断言，避免
    // 将来 users 非空时错位到用户表（560）。
    const tables = w.findAllComponents(NDataTable)
    expect(tables[tables.length - 1]!.props('scrollX')).toBe(640)
  })

  it('PAT 吊销：NPopconfirm 确认 → DELETE /auth/tokens/{id}（204）+ 刷新', async () => {
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'GET',
      '/api/v1/auth/tokens',
      jsonResponse(200, [{ id: 7, name: 'ci-deploy', expires_at: null, created_at: 0 }]),
    )
    setRoute('DELETE', '/api/v1/auth/tokens/', noContent())
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('ci-deploy'))

    // 刷新后令牌列表为空。
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))

    await w.get('button[name="token-revoke"]').trigger('click')
    await confirmPopconfirm()

    await vi.waitFor(() => {
      const del = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'DELETE',
      ) as [string, RequestInit]
      expect(del[0]).toBe('/api/v1/auth/tokens/7')
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('令牌已吊销'))
    await vi.waitFor(() => expect(w.text()).not.toContain('ci-deploy'))
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染用户表/PAT 区', async () => {
    // /users 403；/auth/tokens owner-only 仍 200（但 adminOnly 隐藏整页）。
    setRoute('GET', '/api/v1/users', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('仅全局管理员可见'))
    expect(w.findAll('.n-card')).toHaveLength(0)
    expect(w.find('.n-data-table').exists()).toBe(false)
  })
})

// 用户 / PAT 页行为测试（ADR-0014，票 B4-T6）：只测外部行为，API 层以 fetch
// mock（method + URL 前缀路由，最长前缀匹配）驱动。视图在 onMounted 即并发
// 加载用户与 PAT：mount 须在设置 fetch mock 之后。
// - 用户清单 + admin/普通 badge + 状态
// - 建号：POST /users { username, password, is_admin }（建号时设全局 admin）+ 刷新
// - 禁用/启用：PATCH /users/{name} { disabled }；重置密码：PUT /users/{name}/password
// - PAT：创建 POST /auth/tokens → 一次性明文令牌 + 刷新；吊销 DELETE /auth/tokens/{id}
// - 403 → admin-only 退化态

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

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

describe('UsersView 用户生命周期 + PAT 一次明文', () => {
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
    return mount(UsersView, { global: { plugins: [pinia, router, i18n] } })
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
    vi.restoreAllMocks()
  })

  it('加载用户清单 + admin/普通 badge + 状态徽标', async () => {
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
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.user-table tbody tr').length).toBe(3))

    expect(wrapper.text()).toContain('全局管理员') // admin badge
    expect(wrapper.text()).toContain('已禁用') // bob 状态
    wrapper.unmount()
  })

  it('建号：POST /users { username, password, is_admin }（建号时设全局 admin）+ 刷新', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/users',
      jsonResponse(201, user('bob', { id: 9, is_admin: true })),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无用户'))

    // 刷新后清单含新用户。
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('bob', { id: 9, is_admin: true })]))

    await wrapper.get('button[name="user-new"]').trigger('click')
    await wrapper.get('input[name="user-username"]').setValue('bob')
    await wrapper.get('input[name="user-password"]').setValue('pw123456')
    await wrapper.get('input[name="user-is-admin"]').setValue(true)
    await wrapper.get('button[name="user-create"]').trigger('click')

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
    // 刷新后新用户入列。
    await vi.waitFor(() => expect(wrapper.text()).toContain('bob'))
    wrapper.unmount()
  })

  it('建号 409（重名）→ 就地展示错误，表单停留', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'POST',
      '/api/v1/users',
      jsonResponse(409, { code: 'CONFLICT', message: '用户名已存在：bob' }),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无用户'))

    await wrapper.get('button[name="user-new"]').trigger('click')
    await wrapper.get('input[name="user-username"]').setValue('bob')
    await wrapper.get('input[name="user-password"]').setValue('pw123456')
    await wrapper.get('button[name="user-create"]').trigger('click')

    await vi.waitFor(() =>
      expect(wrapper.get('[role="alert"]').text()).toContain('用户名已存在'),
    )
    expect(wrapper.text()).toContain('全局管理员') // 复选框标签 → 表单停留
    wrapper.unmount()
  })

  it('禁用/启用：PATCH /users/{name} { disabled } + 刷新后按钮翻转', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice', { disabled: false })]))
    setRoute(
      'PATCH',
      '/api/v1/users/',
      jsonResponse(200, user('alice', { disabled: true })),
    )
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.user-table tbody tr').length).toBe(1))

    // 初始按钮为「禁用」（disabled=false）。
    expect(wrapper.get('button[name="user-toggle"]').text()).toBe('禁用')

    // 刷新后回读为已禁用。
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice', { disabled: true })]))

    await wrapper.get('button[name="user-toggle"]').trigger('click')
    await vi.waitFor(() => {
      const patch = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PATCH',
      ) as [string, RequestInit]
      expect(patch[0]).toBe('/api/v1/users/alice')
      expect(JSON.parse(patch[1].body as string)).toEqual({ disabled: true })
    })
    await vi.waitFor(() => expect(wrapper.get('button[name="user-toggle"]').text()).toBe('启用'))
    wrapper.unmount()
  })

  it('重置密码：PUT /users/{name}/password { new_password }（204）', async () => {
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    setRoute('GET', '/api/v1/users', jsonResponse(200, [user('alice')]))
    setRoute('PUT', '/api/v1/users/', noContent())
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('alice'))

    await wrapper.get('button[name="user-reset"]').trigger('click')
    await wrapper.get('input[name="reset-password"]').setValue('newpw123')
    await wrapper.get('button[name="reset-submit"]').trigger('click')

    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PUT',
      ) as [string, RequestInit]
      expect(put[0]).toBe('/api/v1/users/alice/password')
      expect(JSON.parse(put[1].body as string)).toEqual({ new_password: 'newpw123' })
    })
    // 204 后行内表单收起。
    await vi.waitFor(() => expect(wrapper.find('input[name="reset-password"]').exists()).toBe(false))
    wrapper.unmount()
  })

  it('PAT：创建 POST /auth/tokens → 一次性明文令牌 + 刷新列表', async () => {
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
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无令牌'))

    // 刷新后 PAT 列表含新令牌。
    setRoute(
      'GET',
      '/api/v1/auth/tokens',
      jsonResponse(200, [
        { id: 7, name: 'ci-deploy', expires_at: null, created_at: 1_700_000_000_000 },
      ]),
    )

    await wrapper.get('button[name="token-new"]').trigger('click')
    await wrapper.get('input[name="token-name"]').setValue('ci-deploy')
    await wrapper.get('button[name="token-create"]').trigger('click')

    // 提交形态：POST /api/v1/auth/tokens，留空过期 = 不带 expires_at。
    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'POST' && c[0] === '/api/v1/auth/tokens',
      ) as [string, RequestInit]
      expect(JSON.parse(post[1].body as string)).toEqual({ name: 'ci-deploy' })
    })

    // 一次性明文令牌仅此一次展示 + 警示。
    await vi.waitFor(() => expect(wrapper.text()).toContain('sis_ABCDEFGHIJK_one_time'))
    expect(wrapper.text()).toContain('此后任何端点都无法找回')

    // 丢弃后令牌不再可见。
    await wrapper.get('button[name="token-dismiss"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).not.toContain('sis_ABCDEFGHIJK_one_time'))
    wrapper.unmount()
  })

  it('PAT 吊销：DELETE /auth/tokens/{id}（204）+ 刷新', async () => {
    setRoute('GET', '/api/v1/users', jsonResponse(200, []))
    setRoute(
      'GET',
      '/api/v1/auth/tokens',
      jsonResponse(200, [{ id: 7, name: 'ci-deploy', expires_at: null, created_at: 0 }]),
    )
    setRoute('DELETE', '/api/v1/auth/tokens/', noContent())
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('ci-deploy'))

    // 刷新后令牌列表为空。
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))

    await wrapper.get('button[name="token-revoke"]').trigger('click')
    await vi.waitFor(() => {
      const del = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'DELETE',
      ) as [string, RequestInit]
      expect(del[0]).toBe('/api/v1/auth/tokens/7')
    })
    await vi.waitFor(() => expect(wrapper.text()).not.toContain('ci-deploy'))
    wrapper.unmount()
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染用户表/PAT 区', async () => {
    // /users 403；/auth/tokens owner-only 仍 200（但 adminOnly 隐藏整页）。
    setRoute('GET', '/api/v1/users', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    setRoute('GET', '/api/v1/auth/tokens', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('仅全局管理员可见'))
    expect(wrapper.findAll('.users-section').length).toBe(0)
    wrapper.unmount()
  })
})

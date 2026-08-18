// 路由守卫行为测试（ADR-0014，票 B4-T1 骨架）：
// - 会话恢复锚点：状态未知时经 `/auth/me`（restore）恢复
// - 未认证访问受保护页 → 登录页 + redirect 回跳参数
// - 已登录访问 login/setup → 回首页
// - 公开路由放行、已认证访问受保护页放行
// 只测外部行为（导航返回值），不经真实浏览器路由。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type { RouteLocationNormalized } from 'vue-router'

import { sessionGuard } from '@/router/guards'
import { useAuthStore } from '@/stores/auth'

/** 构造最小 RouteLocation 壳（守卫只消费 meta/name/fullPath）。 */
function route(overrides: Partial<RouteLocationNormalized> = {}): RouteLocationNormalized {
  return {
    path: '/',
    fullPath: '/',
    name: undefined,
    meta: {},
    matched: [],
    hash: '',
    query: {},
    params: {},
    redirectedFrom: undefined,
    ...overrides,
  } as unknown as RouteLocationNormalized
}

describe('sessionGuard', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    // 守卫内部经 auth.restore() 调 /auth/me（真实 fetch）——测试里 mock。
    globalThis.fetch = vi.fn()
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('状态未知时先经 /auth/me 恢复（会话恢复锚点）', async () => {
    const auth = useAuthStore()
    expect(auth.status).toBe('unknown')

    // /auth/me 返回 200（已登录）。
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ username: 'alice', is_admin: true }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    )

    const result = await sessionGuard(route({ name: 'overview', meta: {} }))
    expect(result).toBe(true)
    expect(auth.isAuthed).toBe(true)
  })

  it('未认证访问受保护页 → 重定向登录 + redirect 回跳参数', async () => {
    const auth = useAuthStore()
    auth.status = 'guest'
    auth.user = null

    const to = route({ name: 'build-detail', fullPath: '/projects/a/pipelines/p/builds/42', meta: {} })
    const result = await sessionGuard(to)
    expect(result).toEqual({
      name: 'login',
      query: { redirect: '/projects/a/pipelines/p/builds/42' },
    })
  })

  it('server 不可达（unreachable）访问受保护页 → 放行（不弹登录，网络面非认证面）', async () => {
    const auth = useAuthStore()
    auth.status = 'unreachable'
    auth.user = null

    const to = route({ name: 'projects', fullPath: '/projects', meta: {} })
    const result = await sessionGuard(to)
    expect(result).toBe(true)
  })

  it('已登录访问受保护页 → 放行', async () => {
    const auth = useAuthStore()
    auth.status = 'authed'
    auth.user = { username: 'alice', isAdmin: false }

    const result = await sessionGuard(route({ name: 'projects', meta: {} }))
    expect(result).toBe(true)
  })

  it('已登录访问 login/setup → 回首页（认证面不留空屏）', async () => {
    const auth = useAuthStore()
    auth.status = 'authed'
    auth.user = { username: 'alice', isAdmin: false }

    const loginResult = await sessionGuard(route({ name: 'login', meta: { public: true } }))
    expect(loginResult).toEqual({ name: 'overview' })

    const setupResult = await sessionGuard(route({ name: 'setup', meta: { public: true } }))
    expect(setupResult).toEqual({ name: 'overview' })
  })

  it('公开路由（login/setup/404）未登录时放行', async () => {
    const auth = useAuthStore()
    auth.status = 'guest'
    auth.user = null

    for (const name of ['login', 'setup', 'not-found']) {
      const result = await sessionGuard(route({ name, meta: { public: true } }))
      expect(result).toBe(true)
    }
  })
})

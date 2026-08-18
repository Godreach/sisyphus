// 认证态 store 行为测试（ADR-0014/0010，票 B4-T1 会话恢复锚点 + B4-T2
// 登录/登出/空库判定）：
// - `/auth/me` 200 → authed（会话恢复）
// - `/auth/me` 401 → guest（确认未登录）
// - 网络层失败 → unreachable（server 不可达 ≠ 未登录，守卫据此不弹登录）
// - `login` 换 cookie 会话并写用户；`logout` 清认证态
// - `isSetupNeeded` 空库判定：`POST /auth/setup` 200 → true、404 → false
//   （引导已完成）、网络失败 → false、已登录 → false、dismiss 后 → false
// 只测外部行为（status 状态迁移），fetch 以 mock 驱动。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

import { useAuthStore } from '@/stores/auth'

/** 构造 mock JSON 响应（jsdom 无 fetch，需自造 Response 壳）。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

describe('auth store 会话恢复（/auth/me 锚点）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('me 200 → authed，写入用户与管理员标志', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      jsonResponse(200, { username: 'alice', is_admin: true }),
    )
    const auth = useAuthStore()
    const status = await auth.restore()
    expect(status).toBe('authed')
    expect(auth.isAuthed).toBe(true)
    expect(auth.user).toEqual({ username: 'alice', isAdmin: true })
  })

  it('me 401 → guest（确认未登录）', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      jsonResponse(401, { code: 'UNAUTHORIZED', message: '未认证或会话已过期' }),
    )
    const auth = useAuthStore()
    const status = await auth.restore()
    expect(status).toBe('guest')
    expect(auth.isAuthed).toBe(false)
    expect(auth.user).toBeNull()
  })

  it('网络层失败 → unreachable（非「未登录」）', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('Failed to fetch'))
    const auth = useAuthStore()
    const status = await auth.restore()
    expect(status).toBe('unreachable')
    expect(auth.isAuthed).toBe(false)
  })
})

describe('auth store 登录/登出闭环（B4-T2）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('login 成功换会话并写用户（authed）', async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(200, { username: 'alice', is_admin: true }),
    )
    globalThis.fetch = fetchMock
    const auth = useAuthStore()
    await auth.login('alice', 'secret123')
    expect(auth.isAuthed).toBe(true)
    expect(auth.user).toEqual({ username: 'alice', isAdmin: true })

    // 请求形态：POST /api/v1/auth/login，JSON 凭据体。
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/auth/login')
    expect(init.method).toBe('POST')
    expect(JSON.parse(init.body as string)).toEqual({ username: 'alice', password: 'secret123' })
  })

  it('login 失败（401）抛错且不置 authed', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      jsonResponse(401, { code: 'UNAUTHORIZED', message: '用户名或密码错误' }),
    )
    const auth = useAuthStore()
    await expect(auth.login('alice', 'wrong')).rejects.toMatchObject({ code: 'UNAUTHORIZED' })
    expect(auth.isAuthed).toBe(false)
  })

  it('logout 调 POST /auth/logout 并清认证态（服务端失败也清本地态）', async () => {
    const auth = useAuthStore()
    auth.status = 'authed'
    auth.user = { username: 'alice', isAdmin: true }

    const fetchMock = vi.fn().mockResolvedValue(new Response(null, { status: 204 }))
    globalThis.fetch = fetchMock
    await auth.logout()
    expect((fetchMock.mock.calls[0] as [string, RequestInit])[0]).toBe('/api/v1/auth/logout')
    expect(auth.isAuthed).toBe(false)
    expect(auth.user).toBeNull()
    expect(auth.status).toBe('guest')
  })
})

describe('auth store 空库判定（B4-T2，ADR-0010）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('POST /auth/setup 422（空库 + 非法输入探测）→ 需引导（true），结果缓存复用', async () => {
    // 用非法输入探测：空库回落 422（不建号），非空库回落 404。
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(422, {
        code: 'VALIDATION_FAILED',
        message: '凭据输入校验失败',
        detail: { errors: [{ path: 'username', message: 'x' }] },
      }),
    )
    globalThis.fetch = fetchMock
    const auth = useAuthStore()
    expect(await auth.isSetupNeeded()).toBe(true)
    // 缓存：不重复探测。
    expect(await auth.isSetupNeeded()).toBe(true)
    expect(fetchMock).toHaveBeenCalledTimes(1)

    // 探测请求体：非法输入（不会真建号）。
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(JSON.parse(init.body as string)).toEqual({ username: '', password: 'x' })
  })

  it('POST /auth/setup 404（非空库）→ 已引导（false）', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      jsonResponse(404, { code: 'NOT_FOUND', message: 'not found' }),
    )
    const auth = useAuthStore()
    expect(await auth.isSetupNeeded()).toBe(false)
    expect(auth.isAuthed).toBe(false)
  })

  it('网络层失败 → false（server 不可达不是「需引导」）', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('Failed to fetch'))
    const auth = useAuthStore()
    expect(await auth.isSetupNeeded()).toBe(false)
  })

  it('已登录不探测（直接 false）', async () => {
    const auth = useAuthStore()
    auth.status = 'authed'
    auth.user = { username: 'alice', isAdmin: false }
    const fetchMock = vi.fn()
    globalThis.fetch = fetchMock
    expect(await auth.isSetupNeeded()).toBe(false)
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('dismiss 后不探测（用户显式离开引导页）', async () => {
    const auth = useAuthStore()
    auth.dismissSetupFlow()
    const fetchMock = vi.fn()
    globalThis.fetch = fetchMock
    expect(await auth.isSetupNeeded()).toBe(false)
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('探测缓存随登录/登出失效：登录后清空库态、登出后重新探测', async () => {
    // 首次探测：空库需引导（缓存 true）。
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(422, { code: 'VALIDATION_FAILED', message: 'x', detail: { errors: [] } }),
    )
    globalThis.fetch = fetchMock
    const auth = useAuthStore()
    expect(await auth.isSetupNeeded()).toBe(true)
    expect(fetchMock).toHaveBeenCalledTimes(1)

    // 登录（用户表已非空）后：isSetupNeeded 直接 false，不重复探测。
    fetchMock.mockResolvedValue(jsonResponse(200, { username: 'alice', is_admin: true }))
    await auth.login('alice', 'secret123')
    expect(await auth.isSetupNeeded()).toBe(false)

    // 登出后：缓存失效，重新探测（世界可能已变）。
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))
    await auth.logout()
    fetchMock.mockResolvedValue(jsonResponse(404, { code: 'NOT_FOUND', message: 'x' }))
    expect(await auth.isSetupNeeded()).toBe(false) // 非空库
    // 调用序：探测(1) + login(2) + logout(3) + 再探测(4)。
    expect(fetchMock).toHaveBeenCalledTimes(4)
  })
})

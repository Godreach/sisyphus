// 认证态 store 行为测试（ADR-0014，票 B4-T1 会话恢复锚点）：
// - `/auth/me` 200 → authed（会话恢复）
// - `/auth/me` 401 → guest（确认未登录）
// - 网络层失败 → unreachable（server 不可达 ≠ 未登录，守卫据此不弹登录）
// 只测外部行为（status 状态迁移），fetch 以 mock 驱动。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

import { useAuthStore } from '@/stores/auth'

describe('auth store 会话恢复（/auth/me 锚点）', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('me 200 → authed，写入用户与管理员标志', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ username: 'alice', is_admin: true }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    )
    const auth = useAuthStore()
    const status = await auth.restore()
    expect(status).toBe('authed')
    expect(auth.isAuthed).toBe(true)
    expect(auth.user).toEqual({ username: 'alice', isAdmin: true })
  })

  it('me 401 → guest（确认未登录）', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ code: 'UNAUTHORIZED', message: '未认证或会话已过期' }), {
        status: 401,
        headers: { 'Content-Type': 'application/json' },
      }),
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

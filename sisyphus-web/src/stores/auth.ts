// 认证态 store（ADR-0014，票 B4-T1 会话恢复锚点 + B4-T2 登录/登出闭环）。
//
// - `me()` 是会话恢复锚点：应用启动与路由守卫经它判定「是否已登录」——
//   `/auth/me` 200 即已登录（会话 cookie 有效），401 即未登录。
// - `restore()` 由 `main.ts` 在挂载前 await：SPA 刷新/深链直接命中
//   /auth/me 恢复认证态，避免「先渲染再弹登录」的闪烁。
// - 401 落登录：API 客户端（`http.ts`）在任意端点 401 时回调
//   `onUnauthorized`，store 清态并跳登录页（带原目标回跳参数）。
//
// 会话状态四态：unknown = 尚未探测；authed = 已登录；guest = 已确认未登录
// （/auth/me 401）；unreachable = 网络层失败（server 不可达）——「网络
// 不通」不是「未登录」，守卫对 unreachable 不弹登录（页面以 NETWORK_ERROR
// 展示），避免 server 重启窗口内把用户误判成登出。

import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { useRouter } from 'vue-router'

import { authApi } from '@/api/client'
import { http } from '@/api/http-singleton'
import { ApiError } from '@/api/types'

export interface AuthUser {
  username: string
  isAdmin: boolean
}

/** 会话恢复的四态：unknown = 尚未探测，authed = 已登录，guest = 未登录，
 *  unreachable = 网络层失败（server 不可达）。 */
export type SessionStatus = 'unknown' | 'authed' | 'guest' | 'unreachable'

export const useAuthStore = defineStore('auth', () => {
  const user = ref<AuthUser | null>(null)
  const status = ref<SessionStatus>('unknown')
  const isAuthed = computed(() => status.value === 'authed')

  /** 会话恢复锚点：调 `/auth/me` 探测登录态。
   *  401（含会话过期）→ guest；网络层失败 → unreachable（非「未登录」）。 */
  async function restore(): Promise<SessionStatus> {
    try {
      const me = await authApi.me()
      user.value = { username: me.username, isAdmin: me.is_admin }
      status.value = 'authed'
    } catch (err) {
      user.value = null
      const isNetwork = err instanceof ApiError && err.status === 0
      status.value = isNetwork ? 'unreachable' : 'guest'
    }
    return status.value
  }

  /** 登录成功：写入用户 + 置 authed（回跳由调用侧/路由守卫处理）。 */
  function setAuthed(next: AuthUser): void {
    user.value = next
    status.value = 'authed'
  }

  /** 登出/401：清认证态（登出由调用侧先调 authApi.logout()）。 */
  function clear(): void {
    user.value = null
    status.value = 'guest'
  }

  /** 跳登录页（带回跳参数）。路由守卫/401 回调共用；组件上下文外（挂载
   *  前恢复期的 401）useRouter 不可用，回落整页导航——两种路径都成立。 */
  function redirectToLogin(redirect?: string): void {
    const target =
      redirect ??
      (typeof window !== 'undefined' ? window.location.pathname + window.location.search : '/')
    let router: ReturnType<typeof useRouter> | null = null
    try {
      router = useRouter()
    } catch {
      router = null
    }
    if (router) {
      router.push({ name: 'login', query: { redirect: target } })
    } else if (typeof window !== 'undefined') {
      const params = new URLSearchParams({ redirect: target })
      window.location.assign(`/login?${params.toString()}`)
    }
  }

  // 401 统一落登录态：单实例客户端挂一次（组件里挂会被热更新重复注册）。
  if (http.onUnauthorized == null) {
    http.onUnauthorized = ({ redirect }) => {
      clear()
      redirectToLogin(redirect)
    }
  }

  return { user, status, isAuthed, restore, setAuthed, clear, redirectToLogin }
})

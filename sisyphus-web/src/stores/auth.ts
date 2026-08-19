// 认证态 store（ADR-0014，票 B4-T1 会话恢复锚点 + B4-T2 登录/登出/空库判定）。
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
//
// 空库判定（ADR-0010，setup wizard 进入条件）：guest（未登录）且用户表为
// 空时，受保护页应先去 `/setup` 引导而非 `/login`。判定经 `POST /auth/setup`
// 探测（Spec B4：无只读「setup 是否需要」端点）：handler 先做空库判定再
// 校验输入——用非法输入探测，空库回落 422（不建号）、非空库回落 404。
// 422/404 非 401，客户端 401 回调不触发；探测结果缓存于 `setupChecked`，
// 完成引导（成功建管理员）或用户显式离开后置 `dismissSetup` 防重复探测。

import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { useRouter } from 'vue-router'

import { authApi, setupApi } from '@/api/client'
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

  /** 空库探测结果缓存（true = 用户表为空需走引导；null = 未探测）。 */
  const setupChecked = ref<boolean | null>(null)
  /** 用户显式离开引导页后置 true：不再把受保护页重定向到 /setup。 */
  const dismissSetup = ref(false)

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

  /** 登录：换会话 cookie（HttpOnly + SameSite=Lax，浏览器自动携带），成功
   *  写用户 + 置 authed（回跳由调用侧/路由守卫处理）。 */
  async function login(username: string, password: string): Promise<void> {
    const me = await authApi.login({ username, password })
    setAuthed({ username: me.username, isAdmin: me.is_admin })
  }

  /** 登出：调 `/auth/logout`（删会话行 + 清 cookie）后清认证态。
   *  登出失败（如会话已失效）仍清本地态——页面不因服务端失败卡死在
   *  登出按钮。 */
  async function logout(): Promise<void> {
    try {
      await authApi.logout()
    } finally {
      clear()
    }
  }

  /** 登录成功：写入用户 + 置 authed（回跳由调用侧/路由守卫处理）。
   *  置 authed 同时清空库探测缓存：建号/登录后用户表已非空，之前的探测
   *  结果（如「空库需引导」）已过期——下次登出后再进受保护页需重新探测。 */
  function setAuthed(next: AuthUser): void {
    user.value = next
    status.value = 'authed'
    setupChecked.value = null
    dismissSetup.value = false
  }

  /** 登出/401：清认证态（登出由调用侧先调 authApi.logout()）。同时清空库
   *  探测缓存与 dismiss——登出后再进受保护页重新探测（世界可能已变）。 */
  function clear(): void {
    user.value = null
    status.value = 'guest'
    setupChecked.value = null
    dismissSetup.value = false
  }

  /**
   * 空库探测：未登录且未探测/未 dismiss 时，经 `POST /auth/setup` 判定
   * 「用户表是否为空」。handler 先做空库判定再校验输入（`api/auth.rs`）：
   * 非空库对任何输入一律 404，空库才继续校验——故用**非法输入**探测，
   * 空库回落 422 而**不建号**（避免用合法探针在空库上真建出个 `__probe__`
   * 管理员的后门副作用）；404 = 非空库 = 引导已完成。已登录用户无需探测。
   * 网络层失败回落 false（server 不可达不是「需要引导」，放行让页面以
   * NETWORK_ERROR 展示——与 unreachable 同语义）。
   */
  async function isSetupNeeded(): Promise<boolean> {
    if (isAuthed.value || dismissSetup.value) return false
    if (setupChecked.value != null) return setupChecked.value
    try {
      await setupApi.setup({ username: '', password: 'x' })
      // 理论不可达：空库 + 非法输入走 422（catch 分支），非空库走 404。
      setupChecked.value = false
    } catch (err) {
      if (err instanceof ApiError) {
        setupChecked.value = err.status === 422 ? true : err.status === 404 ? false : null
      } else {
        setupChecked.value = null
      }
    }
    return setupChecked.value ?? false
  }

  /** 显式离开引导页（跳过全部步骤 / 完成）：置 dismiss，此后守卫不再把
   *  受保护页重定向到 /setup（用户已表态）。 */
  function dismissSetupFlow(): void {
    dismissSetup.value = true
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
  //
  // 仅当 401 前态为 authed（使用中会话过期）才回跳登录——这是 onUnauthorized
  // 的真正语义：用户「以为还登录着」时被服务端拒绝，应落回登录页重试。对
  // boot 探测（status=unknown，restore() 内 me() 401）与已 guest 态不回跳：
  // me() 401 是「未登录」的正常态，路由由守卫按结果态裁决（含空库→/setup 引
  // 导、guest→/login）。若 boot 探测也回跳，则 guest 直访 /login 会在
  // redirectToLogin 的 window.location.assign（挂载前 useRouter 不可用）里
  // 把 /login?redirect=/login 无限嵌套重载——jsdom 的 location.assign 是空
  // 操作测不出，真实浏览器（headless 冒烟）才暴露（票 B4-T9）。
  if (http.onUnauthorized == null) {
    http.onUnauthorized = ({ redirect }) => {
      const wasAuthed = status.value === 'authed'
      clear()
      if (wasAuthed) {
        redirectToLogin(redirect)
      }
    }
  }

  return {
    user,
    status,
    isAuthed,
    setupChecked,
    dismissSetup,
    restore,
    login,
    logout,
    setAuthed,
    clear,
    isSetupNeeded,
    dismissSetupFlow,
    redirectToLogin,
  }
})

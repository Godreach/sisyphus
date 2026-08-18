// 路由守卫（ADR-0014，票 B4-T1 骨架）：
//
// - `sessionGuard`：会话恢复锚点——导航时若认证态未知，先调 `/auth/me`
//   恢复（SPA 刷新/深链直接命中）；未认证访问受保护页重定向登录（带回跳
//   参数），登录成功回跳原目标（LoginView 消费 `route.query.redirect`）。
//   已登录访问 login/setup 直接回首页（认证面不留空屏）。
// - 空库自动判定（guest 访问受保护页 → 若 DB 空则先去 `/setup`）属
//   B4-T2 setup wizard 的实现细节（需 `POST /auth/setup` 404 探测），
//   本票不越界。

import type { NavigationGuardReturn, RouteLocationNormalized } from 'vue-router'

import { useAuthStore } from '@/stores/auth'

/**
 * 守卫函数签名：只消费 `to`（导航目标）。Vue Router 以 (to, from, next)
 * 调用本守卫（参数少的结构兼容），返回值与 `NavigationGuardReturn` 同形态
 * ——显式声明窄签名让测试可直接单参驱动。
 */
export type GuardFn = (to: RouteLocationNormalized) => NavigationGuardReturn | Promise<NavigationGuardReturn>

/** 目标是否公开路由（login / setup / 404 不设认证门槛）。 */
function isPublic(to: RouteLocationNormalized): boolean {
  return to.meta.public === true
}

/**
 * 会话守卫：导航到受保护页前确保认证态已判定；未登录重定向登录页，
 * 带 `redirect` 查询参数供登录成功回跳。
 */
export const sessionGuard: GuardFn = async (to) => {
  const auth = useAuthStore()

  // 认证态未知（首次导航 / 刷新）：经 `/auth/me` 恢复（会话恢复锚点）。
  if (auth.status === 'unknown') {
    await auth.restore()
  }

  if (isPublic(to)) {
    // 已登录用户访问 /login 或 /setup：直接回首页（认证面不留空屏）。
    if (auth.isAuthed && (to.name === 'login' || to.name === 'setup')) {
      return { name: 'overview' }
    }
    return true
  }

  if (!auth.isAuthed) {
    // 未认证访问受保护页 → 登录页，携带原目标供回跳。
    // server 不可达（unreachable）时不弹登录：网络不通不是「未登录」，
    // 放行让页面以 NETWORK_ERROR 展示（运维面，非认证面）。
    if (auth.status === 'unreachable') {
      return true
    }
    return {
      name: 'login',
      query: {
        redirect: to.fullPath,
      },
    }
  }

  return true
}

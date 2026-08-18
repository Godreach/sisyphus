// 端点级 API 客户端（票 B4-T1：工程底座把认证面端点立起来，其余端点在
// 各页面票按需扩展）。消费既有 REST 契约（ADR-0005：`/api/v1/` 前缀），
// 错误形态统一由 `http.ts` 落 `ApiError`（code/message/detail，按 code 分支）。

import { http } from './http-singleton'
import type { CredentialsRequest, MeResponse } from './http'

/** 认证端点（后端 `api/auth.rs`，ADR-0014）。 */
export const authApi = {
  /** 登录：用户名密码换会话 cookie，返回当前用户。 */
  login: (req: CredentialsRequest) =>
    http.post<MeResponse>('auth/login', { json: req }),

  /** 登出：删会话行 + 清 cookie（Bearer 通道无事可做，同 204）。 */
  logout: () => http.post<void>('auth/logout'),

  /** 会话恢复锚点：返回当前用户名与全局管理员标志（401 即未登录）。 */
  me: () => http.get<MeResponse>('auth/me'),
}

/** 初始化引导（setup wizard）：空库时创建首个全局管理员（404 = 已完成）。 */
export const setupApi = {
  setup: (req: CredentialsRequest) =>
    http.post<MeResponse>('auth/setup', { json: req }),
}

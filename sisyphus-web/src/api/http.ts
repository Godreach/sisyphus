// 单实例 API 客户端核心（ADR-0005/0014，Spec B2a §4，票 B4-T1）。
//
// - 凭据双通道：cookie 会话（浏览器自动携带 HttpOnly + SameSite=Lax 会话
//   cookie）与可选 `Authorization: Bearer <PAT>` 头（脚本/CLI 场景经
//   setPat 注入，ADR-0014「Bearer 面 CSRF 天然免疫」）。
// - 统一错误形态：非 2xx 一律解析为 `ErrorBody`（code/message/detail）并
//   抛 `ApiError`；校验失败清单 `detail.errors` 按字段路径定位消费。
// - 401 统一落登录态：会话失效/PAT 吊销/用户禁用时清认证态并回跳登录页
//   （登录与 setup 请求自身除外，避免登录失败死循环）。
// - 网络层失败（无响应）以 `code = NETWORK_ERROR` 形态抛出，UI 可稳定分支。
// - `me()` 是会话恢复锚点（路由守卫用，见 auth store）。

import { ApiError, NETWORK_ERROR_CODE, type ErrorBody } from './types'

export { ApiError, NETWORK_ERROR_CODE }
export type { ErrorBody, ValidationIssue } from './types'

export interface HttpOptions {
  /** 请求体（JSON 序列化）。 */
  json?: unknown
  /** 是否附加 `Authorization: Bearer <PAT>`（默认按已注入 PAT 自动附加）。 */
  bearer?: boolean
  /** 附加请求头。 */
  headers?: Record<string, string>
  /** 是否携带凭据（cookie 会话；默认 true）。 */
  credentials?: RequestCredentials
  /** 请求体原始字节（用于文件上传等，跳过 JSON 序列化）。 */
  body?: BodyInit
  /** 覆盖默认的 method（GET/POST/PUT/PATCH/DELETE）。 */
  method?: string
  /** 查询参数（拼到 path 上；string/number/boolean 直传，null/undefined/空串
   *  剔除——构建列表分页/状态过滤消费，票 B4-T4）。 */
  query?: Record<string, string | number | boolean | null | undefined>
}

export interface ApiClient {
  /** 注入/清除 PAT（Bearer 双通道；null 即回落到纯 cookie 面）。 */
  setPat: (pat: string | null) => void
  /** 401 时的统一回调（认证态清理 + 登录回跳），由 auth store 注册。 */
  onUnauthorized: ((info: { redirect: string }) => void) | null
  /** 底层 fetch：附加双通道凭据 + JSON 解析 + 统一错误。 */
  request: <T>(path: string, options?: HttpOptions) => Promise<T>
  /** 便捷方法。 */
  get: <T>(path: string, options?: HttpOptions) => Promise<T>
  post: <T>(path: string, options?: HttpOptions) => Promise<T>
  put: <T>(path: string, options?: HttpOptions) => Promise<T>
  patch: <T>(path: string, options?: HttpOptions) => Promise<T>
  del: <T>(path: string, options?: HttpOptions) => Promise<T>
}

/** 相对路径统一补 `/api/v1/` 前缀（单一事实源，不散落拼接）。 */
export function apiPath(path: string): string {
  const trimmed = path.startsWith('/') ? path.slice(1) : path
  return `/api/v1/${trimmed}`
}

/** 查询参数拼接到 URL（null/undefined/空串剔除；数组不支持——列表端点
 *  都是单值过滤）。value 为布尔时转字符串（`true`/`false`）。 */
export function withQuery(
  path: string,
  query?: Record<string, string | number | boolean | null | undefined>,
): string {
  if (!query) return path
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(query)) {
    if (value == null || value === '') continue
    search.set(key, String(value))
  }
  const qs = search.toString()
  return qs ? `${path}?${qs}` : path
}

/** 解析非 2xx 响应为统一错误形态；解析失败（形态异常）回落 NETWORK_ERROR。 */
export async function parseErrorResponse(
  res: Response,
  fallbackMessage: string,
): Promise<ApiError> {
  let body: ErrorBody | null = null
  try {
    const text = await res.text()
    if (text) {
      const parsed = JSON.parse(text) as ErrorBody
      if (typeof parsed?.code === 'string' && typeof parsed?.message === 'string') {
        body = parsed
      }
    }
  } catch {
    // 非 JSON 响应体：回落默认形态
  }
  return new ApiError(
    res.status,
    body?.code ?? 'HTTP_ERROR',
    body?.message ?? fallbackMessage,
    body?.detail ?? null,
  )
}

/**
 * 构造单实例 API 客户端。模块级单例由 `client.ts` 导出；`onUnauthorized`
 * 由认证态 store 注入（路由守卫与 401 落登录共用同一锚点）。
 */
export function createApiClient(): ApiClient {
  let pat: string | null = null
  let onUnauthorizedCb: ApiClient['onUnauthorized'] = null

  async function request<T>(path: string, options: HttpOptions = {}): Promise<T> {
    const {
      json,
      bearer = true,
      headers,
      credentials = 'include',
      body,
      method,
      query,
    } = options

    const headersOut: Record<string, string> = {
      ...(json !== undefined ? { 'Content-Type': 'application/json' } : {}),
      ...(bearer && pat ? { Authorization: `Bearer ${pat}` } : {}),
      ...headers,
    }

    let res: Response
    try {
      res = await fetch(withQuery(apiPath(path), query), {
        method: method ?? (json !== undefined || body !== undefined ? 'POST' : 'GET'),
        headers: headersOut,
        credentials,
        body: body ?? (json !== undefined ? JSON.stringify(json) : undefined),
      })
    } catch {
      // 网络层失败（断网 / CORS / server 未起）：统一 NETWORK_ERROR 形态，
      // 非 401 路径，不落登录态——server 可达性问题是运维面不是会话面。
      throw new ApiError(0, NETWORK_ERROR_CODE, '网络请求失败，请检查服务是否可达', null)
    }

    if (res.status === 401) {
      // 会话失效 / PAT 吊销 / 用户禁用：统一清认证态并回跳登录。
      // 登录与 setup 自身失败除外（否则登录失败会把自己弹回登录页）。
      const isAuthRoute = path === 'auth/login' || path === 'auth/setup'
      if (!isAuthRoute) {
        const redirect = typeof window !== 'undefined' ? `${window.location.pathname}${window.location.search}` : '/'
        onUnauthorizedCb?.({ redirect })
      }
    }

    if (!res.ok) {
      const fallback = `请求失败（HTTP ${res.status}）`
      throw await parseErrorResponse(res, fallback)
    }

    if (res.status === 204) {
      return undefined as T
    }

    const text = await res.text()
    if (!text) {
      return undefined as T
    }
    return JSON.parse(text) as T
  }

  const client: ApiClient = {
    setPat: (p: string | null) => {
      pat = p
    },
    get onUnauthorized() {
      return onUnauthorizedCb
    },
    set onUnauthorized(cb) {
      onUnauthorizedCb = cb
    },
    request,
    get: <T>(path: string, options?: HttpOptions) =>
      request<T>(path, { ...options, method: 'GET' }),
    post: <T>(path: string, options?: HttpOptions) =>
      request<T>(path, { ...options, method: 'POST' }),
    put: <T>(path: string, options?: HttpOptions) =>
      request<T>(path, { ...options, method: 'PUT' }),
    patch: <T>(path: string, options?: HttpOptions) =>
      request<T>(path, { ...options, method: 'PATCH' }),
    del: <T>(path: string, options?: HttpOptions) =>
      request<T>(path, { ...options, method: 'DELETE' }),
  }

  return client
}

/** 认证端点 DTO（后端 `auth.rs` 的 `MeResponse` / `CredentialsRequest`）。 */
export interface MeResponse {
  username: string
  is_admin: boolean
}

export interface CredentialsRequest {
  username: string
  password: string
}

// API 客户端行为测试（票 B4-T1）：只测外部行为——网络请求形态、错误
// 形态解析、双通道凭据、401 统一落登录回调。用 `createApiClient()` 工厂
// （不经模块级单例，避免跨用例 PAT/回调状态泄漏），fetch 以 mock 驱动。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { createApiClient, apiPath, NETWORK_ERROR_CODE, parseErrorResponse } from '@/api/http'
import type { ApiClient } from '@/api/http'
import { ApiError } from '@/api/types'
/** 构造一个 mock Response（jsdom 无 fetch，需自造 Response 壳）。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

/** 断言 catch 到的是 ApiError（TS 收窄 + 语义化断言）。 */
function expectApiError(promise: Promise<unknown>): Promise<ApiError> {
  return promise.then(
    () => {
      throw new Error('expected request to reject with ApiError')
    },
    (err: unknown) => {
      expect(err).toBeInstanceOf(ApiError)
      return err as ApiError
    },
  )
}

describe('apiPath', () => {
  it('统一补 /api/v1/ 前缀（单一事实源）', () => {
    expect(apiPath('auth/me')).toBe('/api/v1/auth/me')
    expect(apiPath('/auth/me')).toBe('/api/v1/auth/me')
  })
})

describe('parseErrorResponse', () => {
  it('解析统一错误形态 code/message/detail', async () => {
    const res = jsonResponse(422, {
      code: 'VALIDATION_FAILED',
      message: 'Pipeline 定义校验失败',
      detail: { errors: [{ path: 'stages[0].jobs[0].name', message: '任务名不能为空' }] },
    })
    const err = await parseErrorResponse(res, 'fallback')
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(422)
    expect(err.code).toBe('VALIDATION_FAILED')
    expect(err.validationIssues).toHaveLength(1)
    expect(err.validationIssues[0]?.path).toBe('stages[0].jobs[0].name')
  })

  it('非 JSON 响应体回落默认形态（不炸）', async () => {
    const res = new Response('Internal Server Error', { status: 500 })
    const err = await parseErrorResponse(res, '请求失败（HTTP 500）')
    expect(err.status).toBe(500)
    expect(err.code).toBe('HTTP_ERROR')
    expect(err.message).toBe('请求失败（HTTP 500）')
  })
})

describe('API 客户端', () => {
  let client: ApiClient
  const fetchMock = vi.fn()

  beforeEach(() => {
    client = createApiClient()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('双通道：cookie 会话凭据 + 可选 Bearer PAT 头', async () => {
    const me = { username: 'alice', is_admin: true }
    // mockResolvedValue 会复用一个 Response 实例（body 只可读一次），
    // 每次调用单独给新实例。
    fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(200, me)))

    // 未注入 PAT：不附 Authorization 头。
    await client.get('auth/me')
    let call = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(call[0]).toBe('/api/v1/auth/me')
    expect(call[1].headers).not.toHaveProperty('Authorization')
    expect(call[1].credentials).toBe('include')

    // 注入 PAT：附加 Bearer 头。
    client.setPat('sis_abc')
    await client.get('auth/me')
    call = fetchMock.mock.calls[1] as [string, RequestInit]
    expect((call[1].headers as Record<string, string>).Authorization).toBe('Bearer sis_abc')

    // 显式关闭 bearer（仅 cookie 面）。
    await client.get('auth/me', { bearer: false })
    call = fetchMock.mock.calls[2] as [string, RequestInit]
    expect((call[1].headers as Record<string, string>).Authorization).toBeUndefined()
  })

  it('JSON 请求体自动序列化 + 正确 method', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { username: 'alice', is_admin: true }))
    await client.post('auth/login', { json: { username: 'alice', password: '12345678' } })
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/auth/login')
    expect(init.method).toBe('POST')
    expect(init.headers).toMatchObject({ 'Content-Type': 'application/json' })
    expect(JSON.parse(init.body as string)).toEqual({ username: 'alice', password: '12345678' })
  })

  it('204 无内容返回 undefined（不解析空体）', async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))
    await expect(client.post('auth/logout')).resolves.toBeUndefined()
  })

  it('非 2xx 抛统一 ApiError（code/message/detail）', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(401, { code: 'UNAUTHORIZED', message: '未认证或会话已过期' }),
    )
    const err = await expectApiError(client.get('auth/me'))
    expect(err.code).toBe('UNAUTHORIZED')
    expect(err.message).toBe('未认证或会话已过期')
  })

  it('401 触发 onUnauthorized 回调（统一落登录态）且登录自身除外', async () => {
    const onUnauthorized = vi.fn()
    client.onUnauthorized = onUnauthorized

    // 受保护端点 401：回调触发。
    fetchMock.mockResolvedValue(jsonResponse(401, { code: 'UNAUTHORIZED', message: 'x' }))
    await client.get('auth/me').catch(() => {})
    expect(onUnauthorized).toHaveBeenCalledOnce()
    expect(onUnauthorized.mock.calls[0]?.[0]).toMatchObject({ redirect: expect.any(String) })

    // 登录端点自身 401：不触发（避免登录失败把自己弹回登录页）。
    onUnauthorized.mockClear()
    fetchMock.mockResolvedValue(jsonResponse(401, { code: 'UNAUTHORIZED', message: 'x' }))
    await client.post('auth/login', { json: { username: 'a', password: 'b' } }).catch(() => {})
    expect(onUnauthorized).not.toHaveBeenCalled()
  })

  it('网络层失败（fetch reject）抛 NETWORK_ERROR 形态、不触发 onUnauthorized', async () => {
    const onUnauthorized = vi.fn()
    client.onUnauthorized = onUnauthorized
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'))
    const err = await expectApiError(client.get('auth/me'))
    expect(err.status).toBe(0)
    expect(err.code).toBe(NETWORK_ERROR_CODE)
    expect(onUnauthorized).not.toHaveBeenCalled()
  })

  it('429 限流错误携带 retryAfterMs', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(429, {
        code: 'RATE_LIMITED',
        message: '登录尝试过于频繁，请稍后再试',
        detail: { retry_after_ms: 30000 },
      }),
    )
    const err = await expectApiError(
      client.post('auth/login', { json: { username: 'a', password: 'b' } }),
    )
    expect(err.code).toBe('RATE_LIMITED')
    expect(err.retryAfterMs).toBe(30000)
  })

  it('query 参数拼接到 URL（null/undefined/空串剔除；布尔转字符串）', async () => {
    // mockImplementation：每次调用给新 Response（本测试两次请求）。
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(200, { items: [], total: 0, page: 1, limit: 20 })),
    )
    await client.get('projects/p/pipelines/pl/builds', {
      query: { page: 2, limit: 20, status: 'failed', empty: '', nil: null, flag: true },
    })
    const [url] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe(
      '/api/v1/projects/p/pipelines/pl/builds?page=2&limit=20&status=failed&flag=true',
    )

    // 无 query 时不带 ?。
    await client.get('auth/me')
    const [url2] = fetchMock.mock.calls[1] as [string, RequestInit]
    expect(url2).toBe('/api/v1/auth/me')
  })

  it('无响应体成功（200 空体）返回 undefined', async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 200 }))
    await expect(client.get('auth/me')).resolves.toBeUndefined()
  })
})

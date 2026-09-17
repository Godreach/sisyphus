// 制品库入口 / S3 配置 mock 契约（票 #122）：未配置时 available=false，
// 凭据不进响应；测试连接未配置 409；配置端点全局 admin。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { server } from '@/mocks/node'

const BASE = '/api/v1'

function json(url: string, method: string, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}${url}`, {
    method,
    headers: { 'x-sisyphus-mock-user': user },
  })
}

describe('制品库 / S3 配置 mock 契约（票 #122）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('未配置时制品库入口 available=false，响应不含凭据', async () => {
    const res = await json('/artifact-repository', 'GET')
    expect(res.status).toBe(200)
    const body = (await res.json()) as { available: boolean; reason?: string }
    expect(body).toEqual({ available: false, reason: 's3_unconfigured' })
    expect(JSON.stringify(body)).not.toContain('secret')
    expect(JSON.stringify(body)).not.toContain('access_key')
  })

  it('脱敏配置仅全局 admin；非 admin 403；响应不含凭据', async () => {
    const admin = await json('/config/s3', 'GET', 'admin')
    expect(admin.status).toBe(200)
    const body = await admin.json()
    expect(body).toEqual({ configured: false })
    expect(JSON.stringify(body)).not.toContain('secret')

    const member = await json('/config/s3', 'GET', 'alice')
    expect(member.status).toBe(403)
  })

  it('未配置时测试连接 409，错误不含凭据', async () => {
    const res = await json('/config/s3/test-connection', 'POST')
    expect(res.status).toBe(409)
    const body = (await res.json()) as { message: string }
    expect(body.message).toContain('未配置')
    expect(JSON.stringify(body)).not.toContain('secret')
  })
})
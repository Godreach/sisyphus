import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { http, HttpResponse } from 'msw'
import { pipelinesApi } from '@/api/client'
import { ApiError } from '@/api/http'
import { server } from '@/mocks/node'

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('流水线条件首建客户端契约（#120）', () => {
  it('仅首建发送 If-None-Match，普通编辑仍是无条件 PUT，412 透传且不覆盖重试', async () => {
    const requests: Request[] = []
    const payload = { name: '中文 名称', parameters: [], env: [], stages: [] }
    server.use(http.put('/api/v1/projects/:project/pipelines/:pipeline', ({ request, params }) => {
      requests.push(request)
      expect(params.project).toBe('项目 A')
      expect(params.pipeline).toBe('中文 名称')
      if (request.headers.has('If-None-Match')) return HttpResponse.json({ code: 'PRECONDITION_FAILED', message: 'already exists' }, { status: 412 })
      return HttpResponse.json({ revision: 2, operator: 'alice', updated_at: 1 })
    }))
    await expect(pipelinesApi.createDefinition('项目 A', '中文 名称', payload)).rejects.toMatchObject({ status: 412, code: 'PRECONDITION_FAILED' } satisfies Partial<ApiError>)
    expect(requests).toHaveLength(1)
    expect(requests[0]!.method).toBe('PUT')
    expect(requests[0]!.headers.get('If-None-Match')).toBe('*')
    const saved = await pipelinesApi.saveDefinition('项目 A', '中文 名称', payload)
    expect(saved.revision).toBe(2)
    expect(requests[1]!.headers.has('If-None-Match')).toBe(false)
  })
})

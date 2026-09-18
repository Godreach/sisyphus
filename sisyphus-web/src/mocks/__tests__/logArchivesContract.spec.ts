// 日志归档 mock 契约（票 #142）：通过公开 HTTP seam 对账单任务状态、
// 管理积压与 mark-lost，避免页面测试绕过真实 API 客户端。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import type { ArchiveStatus } from '@/api/types'
import { server } from '@/mocks/node'

const BASE = '/api/v1'

function json(url: string, method = 'GET', body?: unknown, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}${url}`, {
    method,
    headers: { 'Content-Type': 'application/json', 'x-sisyphus-mock-user': user },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

describe('日志归档 mock 契约（票 #142）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('单任务状态覆盖 pending、ready、lost；有效任务无归档为 null，未知 attempt 为 404', async () => {
    const status = async (build: number, job: string, attempt = 1) => {
      const response = await json(
        `/projects/web-app/pipelines/main/builds/${build}/jobs/${job}/attempts/${attempt}/logs/status`,
      )
      expect(response.status).toBe(200)
      return (await response.json()) as ArchiveStatus | null
    }

    expect((await status(10, 'unit-test'))?.state).toBe('pending')
    expect((await status(11, 'compile'))?.state).toBe('ready')
    expect((await status(9, 'lint'))?.state).toBe('lost')
    expect(await status(12, 'compile')).toBeNull()

    const missing = await json(
      '/projects/web-app/pipelines/main/builds/10/jobs/unit-test/attempts/99/logs/status',
    )
    expect(missing.status).toBe(404)
  })

  it('积压只列 pending/lost，pending 优先，并支持 offset 分页与空列表', async () => {
    const first = await json('/log-archives?agent=build-04')
    expect(first.status).toBe(200)
    const rows = (await first.json()) as ArchiveStatus[]
    expect(rows.map((row) => row.state)).toEqual(['pending', 'lost'])
    expect(rows.map((row) => row.job_id)).toEqual([10001, 10003])

    const next = (await (await json('/log-archives?agent=build-04&offset=1')).json()) as ArchiveStatus[]
    expect(next.map((row) => row.job_id)).toEqual([10003])

    expect(await (await json('/log-archives?agent=build-01')).json()).toEqual([])
    expect(await (await json('/log-archives?agent=missing-agent')).json()).toEqual([])
  })

  it('mark-lost 仅全局 admin，可校验参数/状态，并原子写入可回放审计', async () => {
    expect((await json('/log-archives/10004/1/lost', 'POST', { reason: 'disk gone' }, 'alice')).status).toBe(403)
    expect((await json('/log-archives/10004/1/lost', 'POST', { reason: '   ' })).status).toBe(422)
    expect((await json('/log-archives/10004/1/lost', 'POST', { reason: 'x'.repeat(2001) })).status).toBe(422)
    expect((await json('/log-archives/10002/1/lost', 'POST', { reason: 'ready 不可改写' })).status).toBe(409)

    const marked = await json('/log-archives/10004/1/lost', 'POST', { reason: '  Agent disk destroyed  ' })
    expect(marked.status).toBe(200)
    expect(await marked.json()).toMatchObject({
      job_id: 10004,
      attempt: 1,
      state: 'lost',
      lost_reason: 'Agent disk destroyed',
    })

    const audit = await json('/audit?event=log_archive_lost')
    expect(audit.status).toBe(200)
    const rows = (await audit.json()) as Array<{
      actor: string
      event: string
      project: string | null
      detail: { job_id: number; attempt: number; reason: string }
    }>
    expect(rows[0]).toMatchObject({
      actor: 'admin',
      event: 'log_archive_lost',
      project: null,
      detail: { job_id: 10004, attempt: 1, reason: 'Agent disk destroyed' },
    })
  })
})

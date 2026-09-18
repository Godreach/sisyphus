// 构建详情目录产物集的 MSW 契约（票 #141，ADR-0024）。
// 覆盖列表状态矩阵、异步删除与单文件下载；请求经真实 fetch 进入默认 handlers。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import type { ArtifactSetsResponse, DeletionJobResponse } from '@/api/types'
import { server } from '@/mocks/node'

const BASE = '/api/v1/projects/web-app/pipelines/release/builds'

function headers(user = 'admin'): HeadersInit {
  return { 'x-sisyphus-mock-user': user }
}

async function listSets(number: number) {
  const res = await fetch(`${BASE}/${number}/artifact-sets`, { headers: headers() })
  expect(res.status).toBe(200)
  return (await res.json()) as ArtifactSetsResponse
}

describe('Artifact Set mock 契约（票 #141）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('默认 demo fixture 展示 ready、pending、missing、unavailable 状态', async () => {
    const states = new Set<string>()
    for (const number of [4, 5, 6, 7, 8]) {
      const body = await listSets(number)
      for (const item of body.items) {
        states.add(item.set.state)
        states.add(item.availability)
        for (const entry of item.entries) states.add(entry.state)
      }
    }

    expect([...states]).toEqual(expect.arrayContaining(['ready', 'pending', 'missing', 'unavailable']))
  })

  it('ready 集合文件下载返回稳定正文、内容类型和文件名', async () => {
    const body = await listSets(4)
    const ready = body.items.find((item) => item.set.state === 'ready' && item.availability === 'ready')
    expect(ready).toBeDefined()
    const entry = ready!.entries.find((candidate) => candidate.state === 'ready')
    expect(entry).toBeDefined()

    const res = await fetch(
      `${BASE}/4/artifact-sets/${ready!.set.id}/file?path=${encodeURIComponent(entry!.path)}`,
      { headers: headers() },
    )
    expect(res.status).toBe(200)
    expect(res.headers.get('content-type')).toBe('application/octet-stream')
    expect(res.headers.get('content-disposition')).toContain(`filename="${entry!.path.split('/').pop()}"`)
    expect(await res.text()).toBe(`demo artifact set ${ready!.set.id}: ${entry!.path}\n`)
  })

  it('终态构建删除产物集返回 202；viewer 被拒绝，运行中构建返回 409', async () => {
    const body = await listSets(6)
    const setId = body.items[0]!.set.id
    const accepted = await fetch(`${BASE}/6/artifact-sets/${setId}`, {
      method: 'DELETE',
      headers: headers(),
    })
    expect(accepted.status).toBe(202)
    expect((await accepted.json()) as DeletionJobResponse).toMatchObject({
      scope: 'set',
      state: 'queued',
      set_id: setId,
      pipeline_name: 'release',
      build_number: 6,
    })

    const forbidden = await fetch(`${BASE}/6/artifact-sets/${setId}`, {
      method: 'DELETE',
      headers: headers('bob'),
    })
    expect(forbidden.status).toBe(403)

    const pending = await listSets(8)
    const pendingId = pending.items[0]!.set.id
    const conflict = await fetch(`${BASE}/8/artifact-sets/${pendingId}`, {
      method: 'DELETE',
      headers: headers(),
    })
    expect(conflict.status).toBe(409)
  })
})

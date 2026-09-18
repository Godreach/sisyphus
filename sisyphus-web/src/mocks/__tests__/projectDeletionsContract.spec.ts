// 项目异步删除 mock 契约（票 #139）：经真实 HTTP + MSW handler 验证，
// 不读取 mock 内部状态。行为与 server projects.rs / deletions.rs 保持同形。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import type { DeletionJobResponse, ProjectResponse } from '@/api/types'
import { server } from '@/mocks/node'

const BASE = '/api/v1'

function json(url: string, method: string, body?: unknown, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}${url}`, {
    method,
    headers: { 'Content-Type': 'application/json', 'x-sisyphus-mock-user': user },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

describe('项目异步删除 mock 契约（票 #139）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('删除项目返回 queued 任务，项目立即隐藏且任务进入全局清理队列', async () => {
    const name = 'delete-demo-contract'
    const created = await json('/projects', 'POST', {
      name,
      scm_type: 'none',
      scm_url: '',
      default_branch: null,
      scm_username: null,
      scm_password: null,
    })
    expect(created.status).toBe(201)

    const removed = await json(`/projects/${name}`, 'DELETE')
    expect(removed.status).toBe(202)
    const job = (await removed.json()) as DeletionJobResponse
    expect(job).toMatchObject({ project_name: name, scope: 'project', state: 'queued' })

    const projects = (await (await json('/projects', 'GET')).json()) as ProjectResponse[]
    expect(projects.some((project) => project.name === name)).toBe(false)

    const queue = (await (await json('/project-deletions', 'GET')).json()) as {
      items: DeletionJobResponse[]
    }
    expect(queue.items.some((item) => item.id === job.id && item.state === 'queued')).toBe(true)
  })

  it('刷新队列推进全部状态，失败任务显示原因且重试后可完成', async () => {
    const successfulName = 'delete-lifecycle-contract'
    expect((await json('/projects', 'POST', {
      name: successfulName,
      scm_type: 'none',
      scm_url: '',
      default_branch: null,
    })).status).toBe(201)
    const accepted = await json(`/projects/${successfulName}`, 'DELETE')
    const successful = (await accepted.json()) as DeletionJobResponse

    const states: string[] = []
    for (let poll = 0; poll < 3; poll += 1) {
      const body = (await (await json('/project-deletions', 'GET')).json()) as {
        items: DeletionJobResponse[]
      }
      states.push(body.items.find((item) => item.id === successful.id)?.state ?? 'missing')
    }
    expect(states).toEqual(['queued', 'running', 'completed'])

    const failedAccepted = await json('/projects/deletion-failure-demo', 'DELETE')
    expect(failedAccepted.status).toBe(202)
    const failedJob = (await failedAccepted.json()) as DeletionJobResponse
    let failed: DeletionJobResponse | undefined
    for (let poll = 0; poll < 3; poll += 1) {
      const body = (await (await json('/project-deletions', 'GET')).json()) as {
        items: DeletionJobResponse[]
      }
      failed = body.items.find((item) => item.id === failedJob.id)
    }
    expect(failed).toMatchObject({ state: 'failed', attempts: 1 })
    expect(failed?.last_error).toContain('object delete denied')

    const retried = await json(`/project-deletions/${failedJob.id}/retry`, 'POST')
    expect(retried.status).toBe(202)
    expect(await retried.json()).toMatchObject({ state: 'queued', last_error: null })

    const retriedStates: string[] = []
    for (let poll = 0; poll < 3; poll += 1) {
      const body = (await (await json('/project-deletions', 'GET')).json()) as {
        items: DeletionJobResponse[]
      }
      retriedStates.push(body.items.find((item) => item.id === failedJob.id)?.state ?? 'missing')
    }
    expect(retriedStates).toEqual(['queued', 'running', 'completed'])
  })

  it('全局管理员权限、运行中构建冲突和未知资源错误与服务端同形', async () => {
    expect((await json('/projects/fresh-project', 'DELETE', undefined, 'alice')).status).toBe(403)
    expect((await json('/project-deletions', 'GET', undefined, 'alice')).status).toBe(403)
    expect((await json('/project-deletions/999999/retry', 'POST', undefined, 'alice')).status).toBe(403)

    const conflict = await json('/projects/web-app', 'DELETE')
    expect(conflict.status).toBe(409)
    expect(await conflict.json()).toMatchObject({ code: 'CONFLICT' })
    const visible = (await (await json('/projects', 'GET')).json()) as ProjectResponse[]
    expect(visible.some((project) => project.name === 'web-app')).toBe(true)

    expect((await json('/projects/unknown-project', 'DELETE')).status).toBe(404)
    expect((await json('/project-deletions/999999/retry', 'POST')).status).toBe(404)
  })
})

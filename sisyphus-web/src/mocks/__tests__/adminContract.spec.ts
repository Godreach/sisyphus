// 机密/审计域 mock 契约对账（票 #110）：mock 行为必须等同后端 REST 契约
// （api/secrets.rs / api/audit.rs）——机密「只记名不记值」（PUT value 即弃、
// 任何响应无值形态）、非法机密名 422（env 键字符集）、删不存在的机密 404、
// 项目 admin 档守卫（无角色 404 同形 / 档位不足 403）；审计仅全局 admin
// （403）、过滤 AND 组合 + limit/offset 分页、时间倒序、动作落审计闭环
// （写/删机密 → 审计可回放）。
//
// node 模式 sessionUser 自 cookie 头读取（authEnforced=false 不校验存在性，
// 只作角色分流）——请求可模拟 alice/bob 等非全局 admin 视角。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { server } from '@/mocks/node'

const BASE = '/api/v1'

function json(url: string, method: string, body?: unknown, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}${url}`, {
    method,
    headers: { 'Content-Type': 'application/json', 'x-sisyphus-mock-user': user },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

describe('机密/审计域 mock 契约（票 #110）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('机密只记名不记值：GET 仅名清单（按名排序）、PUT 无值响应、值不进任何 fixture', async () => {
    // 预置清单按名排序。
    const list = (await (await json('/projects/web-app/secrets', 'GET')).json()) as { name: string }[]
    expect(list).toEqual([
      { name: 'DEPLOY_KEY' },
      { name: 'NPM_TOKEN' },
      { name: 'SSH_HOST_KEY' },
    ])

    // 写入新机密（值在请求体）→ 204 无响应体（无值形态）。
    const put = await json('/projects/web-app/secrets/AAA_NEW', 'PUT', { value: 'super-secret-value' })
    expect(put.status).toBe(204)
    expect(await put.text()).toBe('')

    // 清单只多出名、不含值——「值形态不存在」在 mock 层成立。
    const after = (await (await json('/projects/web-app/secrets', 'GET')).json()) as { name: string }[]
    expect(after.map((s) => s.name)).toContain('AAA_NEW')
    expect(JSON.stringify(after)).not.toContain('super-secret-value')

    // 同名写入 = 覆写（清单长度不变；值仍不可读）。
    const put2 = await json('/projects/web-app/secrets/AAA_NEW', 'PUT', { value: 'v2' })
    expect(put2.status).toBe(204)
    const after2 = (await (await json('/projects/web-app/secrets', 'GET')).json()) as { name: string }[]
    expect(after2.filter((s) => s.name === 'AAA_NEW')).toHaveLength(1)
  })

  it('非法机密名 422（env 键字符集：字母数字 + 下划线）；DELETE 未知名 404', async () => {
    const bad = await json('/projects/web-app/secrets/bad%20name%21', 'PUT', { value: 'v' })
    expect(bad.status).toBe(422)
    const body = (await bad.json()) as { code: string; detail: { errors: { path: string }[] } }
    expect(body.code).toBe('VALIDATION_FAILED')
    expect(body.detail.errors.some((e) => e.path === 'secret')).toBe(true)

    const missing = await json('/projects/web-app/secrets/NOPE', 'DELETE')
    expect(missing.status).toBe(404)
  })

  it('机密项目 admin 档守卫：无角色 404 同形 / viewer·runner 403', async () => {
    // bob 在 mobile-app 无角色 → 404（不可借 403/404 之辨探测存在性）。
    expect((await json('/projects/mobile-app/secrets', 'GET', undefined, 'bob')).status).toBe(404)
    // bob 在 web-app 是 viewer → 403（连名都不可见）。
    expect((await json('/projects/web-app/secrets', 'GET', undefined, 'bob')).status).toBe(403)
    // alice 是 web-app 项目 admin → 200。
    expect((await json('/projects/web-app/secrets', 'GET', undefined, 'alice')).status).toBe(200)
  })

  it('审计仅全局 admin：非 admin 403；admin 过滤/分页/倒序同契约', async () => {
    expect((await json('/audit', 'GET', undefined, 'alice')).status).toBe(403)

    const all = (await (await json('/audit', 'GET')).json()) as { id: number; ts: number }[]
    expect(all.length).toBeGreaterThan(0)
    // 时间倒序（新事件在前，后端保证）。
    for (let i = 1; i < all.length; i++) {
      expect(all[i - 1]!.ts).toBeGreaterThanOrEqual(all[i]!.ts)
    }

    // AND 过滤：user=admin + event=secret_created。
    const filtered = (await (
      await json('/audit?user=admin&event=secret_created', 'GET')
    ).json()) as { actor: string; event: string }[]
    expect(filtered.length).toBeGreaterThan(0)
    expect(filtered.every((e) => e.actor === 'admin' && e.event === 'secret_created')).toBe(true)

    // 分页：limit=2&offset=2 → 与全量第 3/4 条一致。
    const paged = (await (await json('/audit?limit=2&offset=2', 'GET')).json()) as { id: number }[]
    expect(paged.map((e) => e.id)).toEqual([all[2]!.id, all[3]!.id])
  })

  it('审计参数非法 422：未知事件类型 / limit 越界 / offset 负（audit.rs 同形）', async () => {
    const badEvent = await json('/audit?event=secret_value_read', 'GET')
    expect(badEvent.status).toBe(422)
    const badEventBody = (await badEvent.json()) as { detail: { errors: { path: string }[] } }
    expect(badEventBody.detail.errors.some((e) => e.path === 'event')).toBe(true)

    expect((await json('/audit?limit=0', 'GET')).status).toBe(422)
    expect((await json('/audit?limit=201', 'GET')).status).toBe(422)
    expect((await json('/audit?offset=-1', 'GET')).status).toBe(422)
    // 合法边界放行。
    expect((await json('/audit?limit=200', 'GET')).status).toBe(200)
  })

  it('动作落审计闭环：写/删机密 → 审计可回放（detail 只记名，值不出现）', async () => {
    // 动作前基线：无 AAA_NEW 相关条目。
    const before = (await (
      await json('/audit?project=web-app&event=secret_created', 'GET')
    ).json()) as { detail: { secret: string } }[]
    expect(before.some((e) => e.detail.secret === 'AAA_AUDIT')).toBe(false)

    // 写入 + 删除 → 各落一条审计（created / deleted）。
    expect((await json('/projects/web-app/secrets/AAA_AUDIT', 'PUT', { value: 'pv' })).status).toBe(204)
    expect((await json('/projects/web-app/secrets/AAA_AUDIT', 'DELETE')).status).toBe(204)

    const after = (await (
      await json('/audit?project=web-app&event=secret_created', 'GET')
    ).json()) as { detail: { secret: string } }[]
    expect(after.some((e) => e.detail.secret === 'AAA_AUDIT')).toBe(true)

    const deletedRows = (await (
      await json('/audit?project=web-app&event=secret_deleted', 'GET')
    ).json()) as { detail: { secret: string } }[]
    expect(deletedRows.some((e) => e.detail.secret === 'AAA_AUDIT')).toBe(true)
    // 机密审计只记名：任何 detail 不含写入值 'pv'。
    expect(JSON.stringify(deletedRows)).not.toContain('"pv"')
  })
})

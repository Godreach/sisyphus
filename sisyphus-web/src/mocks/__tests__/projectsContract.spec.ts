// 项目域 mock 契约对账（票 #108）：mock 行为必须等同后端 REST 契约——
// 权限判定序列（server policy.rs：无角色 404 同形 / 档位不足 403 / 全局
// admin 隐含项目 admin）、成员整组替换未知用户 400（members.rs:156）、
// 契约先行 PATCH 的校验冻结（空 URL / svn 默认分支 422，create 同规则）。
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

describe('项目域 mock 契约（票 #108）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('GET /projects/{name}：无角色用户 404 同形（B2b-T5 不暴露存在性）', async () => {
    // bob 在 mobile-app 无成员角色——与「项目不存在」同形 404。
    const res = await json('/projects/mobile-app', 'GET', undefined, 'bob')
    expect(res.status).toBe(404)
    // admin（全局）与 web-app 显式成员 bob 可见。
    expect((await json('/projects/mobile-app', 'GET', undefined, 'admin')).status).toBe(200)
    expect((await json('/projects/web-app', 'GET', undefined, 'bob')).status).toBe(200)
  })

  it('项目 admin 档守卫：无角色 404 同形 / 有角色非 admin 403', async () => {
    // bob 在 mobile-app 无角色 → 404（不可借 403/404 之辨探测存在性）。
    expect((await json('/projects/mobile-app/members', 'GET', undefined, 'bob')).status).toBe(404)
    expect(
      (await json('/projects/mobile-app/test-connection', 'POST', undefined, 'bob')).status,
    ).toBe(404)
    // bob 在 web-app 是 viewer（有角色、档位不足）→ 403。
    expect((await json('/projects/web-app/members', 'GET', undefined, 'bob')).status).toBe(403)
    expect(
      (await json('/projects/web-app/test-connection', 'POST', undefined, 'bob')).status,
    ).toBe(403)
    // alice 是 web-app 项目 admin → 200。
    expect((await json('/projects/web-app/members', 'GET', undefined, 'alice')).status).toBe(200)
  })

  it('members 整组替换含不存在用户 → 400（members.rs:156 同义）', async () => {
    const res = await json('/projects/web-app/members', 'PUT', [
      { username: 'ghost', role: 'viewer' },
    ])
    expect(res.status).toBe(400)
    const body = (await res.json()) as { message: string }
    expect(body.message).toContain('用户不存在')
  })

  it('PATCH 校验冻结：空 scm_url 422；svn 项目带默认分支 422（create 同规则）', async () => {
    const r1 = await json('/projects/web-app', 'PATCH', { scm_url: '   ' })
    expect(r1.status).toBe(422)
    const body1 = (await r1.json()) as { code: string; detail: { errors: { path: string }[] } }
    expect(body1.code).toBe('VALIDATION_FAILED')
    expect(body1.detail.errors.some((e) => e.path === 'scm_url')).toBe(true)

    const r2 = await json('/projects/svn-hooks', 'PATCH', { default_branch: 'main' })
    expect(r2.status).toBe(422)
    const body2 = (await r2.json()) as { code: string; detail: { errors: { path: string }[] } }
    expect(body2.code).toBe('VALIDATION_FAILED')
    expect(body2.detail.errors.some((e) => e.path === 'default_branch')).toBe(true)
  })

  it('PATCH 落定：scm_url/default_branch 更新并回读；缺省字段不动', async () => {
    const before = (await (await json('/projects/web-app', 'GET')).json()) as {
      scm_url: string
      default_branch: string | null
    }
    const res = await json('/projects/web-app', 'PATCH', { default_branch: 'develop' })
    expect(res.status).toBe(200)
    const after = (await res.json()) as { scm_url: string; default_branch: string | null }
    expect(after.default_branch).toBe('develop')
    expect(after.scm_url).toBe(before.scm_url) // 未提交字段不动
  })

  it('users/directory：bob（无任何项目 admin 档）403；alice（web-app admin）200', async () => {
    expect((await json('/users/directory', 'GET', undefined, 'bob')).status).toBe(403)
    expect((await json('/users/directory', 'GET', undefined, 'alice')).status).toBe(200)
  })
})

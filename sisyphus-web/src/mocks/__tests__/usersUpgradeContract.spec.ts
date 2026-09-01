// 用户/PAT/升级包域 mock 契约对账（票 #111）：mock 行为必须等同后端 REST
// 契约（api/users.rs / api/tokens.rs / api/upgrade_packages.rs / agents.rs
// 升级指令）——用户管理全局 admin 专属（403）、建号 409 重名/422 校验
// （用户名字符集、密码最小 8 位）、禁用即踢线级联删 PAT、代办重置密码 204；
// PAT 值仅创建响应一次（列表/吊销无值形态）、吊销 id+属主双条件 404 同形
// （不暴露他人令牌存在性）；升级包 raw octet 上传 + X-Sisyphus-Filename 头、
// 文件名不可解析 422、版本窗外 409、全量 issued/skipped 汇总、单台已在目标
// 版本 409；动作落审计闭环。
//
// node 模式 sessionUser 自 cookie 头读取（authEnforced=false 不校验存在性，
// 只作角色分流）——请求可模拟 alice/bob 等非全局 admin 视角。

import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { server } from '@/mocks/node'

const BASE = '/api/v1'

/** 发 JSON API 请求（user 头分流角色视角；body 缺省即无体）。 */
function api(url: string, method: string, body?: unknown, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}${url}`, {
    method,
    headers: { 'Content-Type': 'application/json', 'x-sisyphus-mock-user': user },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
}

/** raw octet 上传（包名在 X-Sisyphus-Filename 头，body 为包字节）。 */
function uploadRaw(filename: string | null, bytes: string, user = 'admin'): Promise<Response> {
  return fetch(`${BASE}/upgrade-packages`, {
    method: 'POST',
    headers: {
      ...(filename == null ? {} : { 'X-Sisyphus-Filename': filename }),
      'x-sisyphus-mock-user': user,
    },
    body: bytes,
  })
}

describe('用户/PAT/升级包域 mock 契约（票 #111）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  // ----- 用户管理（api/users.rs 同形）-----

  it('用户清单仅全局 admin：admin 200（按用户名排序、含 disabled、无密码形态）；非 admin 403', async () => {
    expect((await api('/users', 'GET', undefined, 'alice')).status).toBe(403)

    const list = (await (await api('/users', 'GET')).json()) as {
      username: string
      is_admin: boolean
      disabled: boolean
    }[]
    expect(list.map((u) => u.username)).toEqual([...list.map((u) => u.username)].sort())
    expect(list.some((u) => u.username === 'admin' && u.is_admin)).toBe(true)
    // UserResponse 无密码形态。
    expect(JSON.stringify(list)).not.toMatch(/"password"/)
  })

  it('建号：201 默认普通用户；重名 409；用户名字符集/密码最小 8 位 422', async () => {
    const created = await api('/users', 'POST', {
      username: 'contract-user',
      password: 'contractpass1',
    })
    expect(created.status).toBe(201)
    const row = (await created.json()) as { username: string; is_admin: boolean; disabled: boolean }
    expect(row).toMatchObject({ username: 'contract-user', is_admin: false, disabled: false })

    // 重名 409。
    expect(
      (await api('/users', 'POST', { username: 'contract-user', password: 'contractpass1' })).status,
    ).toBe(409)
    // 用户名字符集 422（空格非法）。
    const badName = await api('/users', 'POST', { username: 'bad name!', password: 'longenough1' })
    expect(badName.status).toBe(422)
    const badNameBody = (await badName.json()) as { detail: { errors: { path: string }[] } }
    expect(badNameBody.detail.errors.some((e) => e.path === 'username')).toBe(true)
    // 密码最小 8 位 422。
    const badPw = await api('/users', 'POST', { username: 'shortpw', password: 'short' })
    expect(badPw.status).toBe(422)
    const badPwBody = (await badPw.json()) as { detail: { errors: { path: string }[] } }
    expect(badPwBody.detail.errors.some((e) => e.path === 'password')).toBe(true)
  })

  it('禁用/启用：PATCH 200 落定；未知用户 404；禁用即踢线级联删该用户全部 PAT', async () => {
    const patched = await api('/users/contract-user', 'PATCH', { disabled: true })
    expect(patched.status).toBe(200)
    expect(((await patched.json()) as { disabled: boolean }).disabled).toBe(true)
    expect((await api('/users/ghost-user', 'PATCH', { disabled: true })).status).toBe(404)

    // 级联踢线：给 contract-user 建 PAT → 禁用 → 其 PAT 清单为空。
    await api('/auth/tokens', 'POST', { name: 'to-be-kicked' }, 'contract-user')
    await api('/users/contract-user', 'PATCH', { disabled: true })
    const pats = (await (
      await api('/auth/tokens', 'GET', undefined, 'contract-user')
    ).json()) as unknown[]
    expect(pats).toHaveLength(0)
  })

  it('代办重置密码：204；密码短于 8 位 422；未知用户 404', async () => {
    expect((await api('/users/bob/password', 'PUT', { new_password: 'resetpass99' })).status).toBe(204)
    const short = await api('/users/bob/password', 'PUT', { new_password: 'short' })
    expect(short.status).toBe(422)
    expect((await api('/users/ghost/password', 'PUT', { new_password: 'resetpass99' })).status).toBe(404)
  })

  // ----- PAT（api/tokens.rs 同形）-----

  it('PAT 值仅创建响应一次：token 形如 sis_+43、列表/吊销无值形态、owner 隔离', async () => {
    const created = await api('/auth/tokens', 'POST', { name: 'contract-token' })
    expect(created.status).toBe(201)
    const row = (await created.json()) as { token: string; name: string }
    expect(row.token).toMatch(/^sis_[a-z2-7]{43}$/)

    // 列表无值形态；且只含 owner 本人行（alice 的行不在 admin 视角）。
    const adminPats = (await (await api('/auth/tokens', 'GET')).json()) as { id: number; name: string }[]
    expect(JSON.stringify(adminPats)).not.toMatch(/sis_/)
    expect(adminPats.some((p) => p.name === 'contract-token')).toBe(true)
    expect(adminPats.some((p) => p.name === 'laptop-cli')).toBe(false)

    // owner 隔离：alice 只见自己的 laptop-cli。
    const alicePats = (await (
      await api('/auth/tokens', 'GET', undefined, 'alice')
    ).json()) as { id: number; name: string }[]
    expect(alicePats.map((p) => p.name)).toEqual(['laptop-cli'])

    // 吊销 id+属主双条件：admin 吊销 alice 的 id → 404 同形（不暴露存在性）。
    const aliceToken = alicePats[0]!
    const revokedOther = await api(`/auth/tokens/${aliceToken.id}`, 'DELETE')
    expect(revokedOther.status).toBe(404)
  })

  it('PAT 创建校验：名空 422；过期时间须晚于当前时间 422', async () => {
    const noName = await api('/auth/tokens', 'POST', { name: '  ' })
    expect(noName.status).toBe(422)
    const past = await api('/auth/tokens', 'POST', { name: 'x-token', expires_at: 1 })
    expect(past.status).toBe(422)
    const pastBody = (await past.json()) as { detail: { errors: { path: string }[] } }
    expect(pastBody.detail.errors.some((e) => e.path === 'expires_at')).toBe(true)
  })

  // ----- 升级包 / 升级指令（api/upgrade_packages.rs、agents.rs 同形）-----

  it('升级包清单/上传/删除：仅全局 admin；文件名不可解析 422；版本窗外 409；201 解析三元组', async () => {
    expect((await api('/upgrade-packages', 'GET', undefined, 'alice')).status).toBe(403)
    expect((await uploadRaw('x.tar.gz', 'x', 'alice')).status).toBe(403)

    // 缺文件名头 422。
    expect((await uploadRaw(null, 'x')).status).toBe(422)
    // 文件名不可解析 422。
    const bad = await uploadRaw('not-a-package.bin', 'x')
    expect(bad.status).toBe(422)
    const badBody = (await bad.json()) as { detail: { errors: { path: string }[] } }
    expect(badBody.detail.errors.some((e) => e.path === 'filename')).toBe(true)
    // 版本窗外 409（< N-1 与 > Server 各一）。
    expect((await uploadRaw('sisyphus-agent-1.2.0-linux-x86_64.tar.gz', 'x')).status).toBe(409)
    expect((await uploadRaw('sisyphus-agent-1.6.0-linux-x86_64.tar.gz', 'x')).status).toBe(409)

    // 合法上传：201 返回解析出的版本/目标三元组/字节数。
    const uploaded = await uploadRaw('sisyphus-agent-1.4.4-linux-aarch64.tar.gz', '0123456789')
    expect(uploaded.status).toBe(201)
    const meta = (await uploaded.json()) as {
      package_name: string
      version: { major: number; minor: number; patch: number }
      target_os: string
      target_arch: string
      size: number
      sha256: string
    }
    expect(meta).toMatchObject({
      package_name: 'sisyphus-agent-1.4.4-linux-aarch64.tar.gz',
      version: { major: 1, minor: 4, patch: 4 },
      target_os: 'linux',
      target_arch: 'aarch64',
      size: 10,
    })
    expect(meta.sha256).toMatch(/^[0-9a-f]{64}$/)

    // 删除：204；未知包 404。
    expect(
      (await api('/upgrade-packages/sisyphus-agent-1.4.4-linux-aarch64.tar.gz', 'DELETE')).status,
    ).toBe(204)
    expect((await api('/upgrade-packages/ghost-pkg.tar.gz', 'DELETE')).status).toBe(404)
  })

  it('全量升级：202 受理摘要（issued/skipped 真值）；未知包 404；目标落定 draining', async () => {
    expect((await api('/agents/upgrade', 'POST', { package_name: 'ghost.tar.gz' })).status).toBe(404)

    const res = await api('/agents/upgrade', 'POST', {
      package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz',
    })
    expect(res.status).toBe(202)
    const summary = (await res.json()) as { package_name: string; issued: number; skipped: number }
    expect(summary).toMatchObject({
      package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz',
      issued: 4, // build-02/04/06/07（未停用且非 1.5.0）
      skipped: 2, // build-01/03 已在目标版本（停用 build-05 不计）
    })

    // 下发目标落定 draining（ADR-0017：收到指令即自排空）。
    const agents = (await (await api('/agents', 'GET')).json()) as {
      name: string
      draining: boolean
      upgrade_phase: string | null
    }[]
    const build02 = agents.find((a) => a.name === 'build-02')
    expect(build02).toMatchObject({ draining: true, upgrade_phase: 'draining' })
  })

  it('单台升级：202 返回落定后的 Agent；未知 Agent 404；已在目标版本 409', async () => {
    const res = await api('/agents/build-07/upgrade', 'POST', {
      package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz',
    })
    expect(res.status).toBe(202)
    const agent = (await res.json()) as { name: string; draining: boolean; upgrade_phase: string | null }
    expect(agent).toMatchObject({ name: 'build-07', draining: true, upgrade_phase: 'draining' })

    expect(
      (await api('/agents/ghost/upgrade', 'POST', { package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz' }))
        .status,
    ).toBe(404)
    // build-01 已在 1.5.0 → 409。
    expect(
      (await api('/agents/build-01/upgrade', 'POST', { package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz' }))
        .status,
    ).toBe(409)
  })

  it('动作落审计闭环：建号/重置密码/PAT 创建吊销/升级包上传删除/升级指令 → 审计可回放', async () => {
    // 动作。
    await api('/users', 'POST', { username: 'audit-user', password: 'auditpass99' })
    await api('/users/audit-user/password', 'PUT', { new_password: 'auditpass98' })
    const pat = (await (await api('/auth/tokens', 'POST', { name: 'audit-token' })).json()) as {
      id: number
    }
    await api(`/auth/tokens/${pat.id}`, 'DELETE')
    await uploadRaw('sisyphus-agent-1.4.4-windows-x86_64.zip', 'zzz')
    await api('/upgrade-packages/sisyphus-agent-1.4.4-windows-x86_64.zip', 'DELETE')
    await api('/agents/upgrade', 'POST', { package_name: 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz' })

    // 各落一条审计（全局域 project=null）。
    for (const [event, detailPart] of [
      ['user_created', 'audit-user'],
      ['password_reset', 'audit-user'],
      ['pat_created', 'audit-token'],
      ['pat_revoked', 'audit-token'],
      ['upgrade_package_uploaded', 'sisyphus-agent-1.4.4-windows-x86_64.zip'],
      ['upgrade_package_deleted', 'sisyphus-agent-1.4.4-windows-x86_64.zip'],
      ['upgrade_command_issued', 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz'],
    ] as const) {
      const rows = (await (
        await api(`/audit?event=${event}`, 'GET')
      ).json()) as { event: string; detail: Record<string, unknown> | null }[]
      expect(
        rows.some((r) => JSON.stringify(r.detail ?? {}).includes(detailPart)),
        `审计含 ${event}(${detailPart})`,
      ).toBe(true)
    }
  })
})

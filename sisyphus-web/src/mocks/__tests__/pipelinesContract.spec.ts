// Pipeline 定义端点 mock 契约对账（票 #109 编辑器闭环）：mock 行为必须等同
// 后端 REST 契约（server api/pipelines.rs）——
// - GET viewer 档：无成员角色与项目/流水线不存在同形 404（不泄露存在性）；
//   定义输出 model JSON 形态（steps tagged union、pipeline 级 env 永发 []、
//   参数 required 永发——编辑器加载即 model 校验同源对账的完整形态）。
// - PUT admin 档：无角色 404 同形 / 档位不足 403；model 校验失败 422 错误清单
//   整组透传（与编辑器本地校验同一 TS 端口——server InvalidDefinition 同义）；
//   形态错 422 path="$"（server parse_body 同形）；upsert revision 语义
//   （新 pipeline 首存=1、已有续存 +1）；保存→重载读回同定义（往返闭环）。
//
// node 模式 sessionUser 自 x-sisyphus-mock-user 头读取（authEnforced=false
// 不校验存在性，只作角色分流）——可模拟 alice/bob 等非全局 admin 视角。

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

/** 一份最小合法定义（model JSON 形态；parameters/env/stages 永发）。 */
function validDefinition(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    name: 'main',
    parameters: [],
    env: [{ name: 'RUST_LOG', value: 'debug' }],
    stages: [
      {
        name: 'build',
        jobs: [
          {
            name: 'compile',
            steps: [{ type: 'shell', config: { command: 'make compile', shell: null } }],
          },
        ],
      },
    ],
    ...overrides,
  }
}

interface DefinitionResponse {
  definition: Record<string, unknown>
  revision: number
  operator: string
  updated_at: number
}

describe('Pipeline 定义端点 mock 契约（票 #109）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('GET 定义：model JSON 形态（env 永发、steps tagged union、参数 required 永发）', async () => {
    const res = await json('/projects/web-app/pipelines/main', 'GET')
    expect(res.status).toBe(200)
    const body = (await res.json()) as DefinitionResponse
    expect(body.revision).toBe(3)
    expect(body.operator).toBe('admin')
    const def = body.definition as {
      name: string
      env: unknown[]
      parameters: { name: string; required: unknown }[]
      stages: { jobs: { steps: { type: string; config: { command: string } }[] }[] }[]
    }
    expect(def.name).toBe('main')
    expect(def.env).toEqual([])
    // fixture 参数 deploy_target 未声明 required → mock 填 required=false（永发语义）。
    expect(def.parameters).toEqual([
      {
        name: 'deploy_target',
        type: 'enum',
        choices: ['staging', 'prod'],
        default: 'staging',
        required: false,
      },
    ])
    const step = def.stages[0]!.jobs[0]!.steps[0]!
    expect(step.type).toBe('shell')
    expect(typeof step.config.command).toBe('string')
  })

  it('GET viewer 档判定：无角色用户 404 同形；流水线不存在 404', async () => {
    // bob 在 mobile-app 无成员角色——与「项目不存在」同形 404。
    expect((await json('/projects/mobile-app/pipelines/main', 'GET', undefined, 'bob')).status).toBe(404)
    // alice 是 web-app admin，但流水线名不存在 → 404。
    expect((await json('/projects/web-app/pipelines/nope', 'GET', undefined, 'alice')).status).toBe(404)
  })

  it('PUT 合法定义 → revision=fixture 基线 +1，重载读回同定义（往返闭环）', async () => {
    const def = validDefinition()
    const put = await json('/projects/web-app/pipelines/release', 'PUT', def, 'alice')
    expect(put.status).toBe(200)
    const saved = (await put.json()) as Omit<DefinitionResponse, 'definition'>
    expect(saved).toMatchObject({ revision: 4, operator: 'alice' })

    const get = await json('/projects/web-app/pipelines/release', 'GET', undefined, 'alice')
    const body = (await get.json()) as DefinitionResponse
    expect(body.revision).toBe(4)
    expect(body.operator).toBe('alice')
    expect(body.definition).toEqual(def)
  })

  it('PUT 新 pipeline（fixture 无名）→ 首存 revision=1（upsert 语义）', async () => {
    const put = await json('/projects/web-app/pipelines/deploy-v2', 'PUT', validDefinition({ name: 'deploy-v2' }))
    expect(put.status).toBe(200)
    const saved = (await put.json()) as Omit<DefinitionResponse, 'definition'>
    expect(saved.revision).toBe(1)

    const get = await json('/projects/web-app/pipelines/deploy-v2', 'GET')
    expect(((await get.json()) as DefinitionResponse).revision).toBe(1)
  })

  it('PUT model 校验失败 → 422 VALIDATION_FAILED 错误清单整组透传', async () => {
    // 空 shell 命令 → shell_command_empty；必填参数无默认 → required_parameter_default。
    const def = validDefinition({
      parameters: [{ name: 'target', type: 'string', required: true }],
      stages: [
        {
          name: 'build',
          jobs: [{ name: 'compile', steps: [{ type: 'shell', config: { command: '', shell: null } }] }],
        },
      ],
    })
    const res = await json('/projects/web-app/pipelines/nightly', 'PUT', def)
    expect(res.status).toBe(422)
    const body = (await res.json()) as {
      code: string
      detail: { errors: { path: string; message: string }[] }
    }
    expect(body.code).toBe('VALIDATION_FAILED')
    const paths = body.detail.errors.map((e) => e.path)
    expect(paths).toContain('parameters[0].target.required')
    expect(paths).toContain('stages[0].jobs[0].steps[0].command')
  })

  it('PUT 请求体形态不符 model → 422 path="$"（server parse_body 同形）', async () => {
    const res = await json('/projects/web-app/pipelines/nightly', 'PUT', { name: 'nightly' })
    expect(res.status).toBe(422)
    const body = (await res.json()) as { code: string; detail: { errors: { path: string }[] } }
    expect(body.code).toBe('VALIDATION_FAILED')
    expect(body.detail.errors[0]!.path).toBe('$')
  })

  it('PUT admin 档守卫：viewer 403 / 无角色 404 同形', async () => {
    // bob 是 web-app viewer（有角色、档位不足）→ 403。
    expect(
      (await json('/projects/web-app/pipelines/main', 'PUT', validDefinition(), 'bob')).status,
    ).toBe(403)
    // bob 在 mobile-app 无角色 → 404（不泄露存在性）。
    expect(
      (await json('/projects/mobile-app/pipelines/main', 'PUT', validDefinition(), 'bob')).status,
    ).toBe(404)
  })
})

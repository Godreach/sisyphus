// 机密页行为测试（ADR-0015，票 B4-T6）：只测外部行为，API 层以 fetch mock
// （method + URL 前缀路由，最长前缀匹配）驱动。视图在 onMounted 即发项目列表
// 请求并经 watch(selectedProject) 加载首项目机密：mount 须在设置 fetch mock 之后。
// - 项目下拉 + 首项目机密名清单（只列名，值任何端点不回显）
// - 写/覆写：PUT /projects/{name}/secrets/{secret} { value }（值不回显）+ 刷新
// - 删：DELETE /projects/{name}/secrets/{secret} + 刷新
// - 切换项目 → 重新加载；422（机密名非法）→ 校验清单；无项目 → 空态

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import SecretsView from '@/views/SecretsView.vue'
import { i18n, setLocale } from '@/i18n'
import type { ProjectResponse } from '@/api/types'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

/** 204 无响应体（204/205 不允许有 body——Response 构造器会拒）。 */
function noContent(): Response {
  return new Response(null, { status: 204 })
}

function project(name: string): ProjectResponse {
  return {
    id: 1,
    name,
    scm_type: 'git',
    scm_url: 'https://example.com/o/r.git',
    default_branch: 'main',
    created_at: 0,
    updated_at: 0,
  }
}

describe('SecretsView 机密只列名 + 写覆写/删 + 切换项目', () => {
  let pinia: Pinia
  let router: Router

  /** method + URL 前缀 → 响应（最长前缀匹配，按 method 分流）。 */
  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = (init?.method ?? 'GET').toUpperCase()
    let best: { len: number; res: Response } | null = null
    for (const [key, res] of routes) {
      const [m, prefix] = [key.slice(0, key.indexOf(' ')), key.slice(key.indexOf(' ') + 1)]
      if (method !== m.toUpperCase()) continue
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best
      ? best.res
      : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${method} ${url}` })
  })

  function setRoute(method: string, prefix: string, res: Response): void {
    routes.set(`${method.toUpperCase()} ${prefix}`, res)
  }

  function mountView(): VueWrapper {
    return mount(SecretsView, { global: { plugins: [pinia, router, i18n] } })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/secrets', name: 'admin-secrets', component: { template: '<div />' } }],
    })
    await router.push('/admin/secrets')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载项目下拉 + 首项目机密名清单（只列名，值不回显）', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, [project('proj-a'), project('proj-b')]))
    setRoute(
      'GET',
      '/api/v1/projects/proj-a/secrets',
      jsonResponse(200, [{ name: 'DEPLOY_KEY' }, { name: 'TOKEN' }]),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.secret-item').length).toBe(2))

    // 项目下拉含两个项目，首项默认选中。
    const options = wrapper.find('select[name="secret-project"]').findAll('option')
    expect(options.map((o) => o.attributes('value'))).toEqual(['proj-a', 'proj-b'])

    // 机密名清单只列名（无值形态展示）。
    expect(wrapper.text()).toContain('DEPLOY_KEY')
    expect(wrapper.text()).toContain('TOKEN')
    // 语义提示在位（值只写不读 + ${} 不解析）。
    expect(wrapper.text()).toContain('值只写不读')
    expect(wrapper.text()).toContain('${}')

    // 提交形态：GET /api/v1/projects/proj-a/secrets（首项目默认选中后经 watch 触发）。
    const secretsGet = fetchMock.mock.calls.find(
      (c) => c[0] === '/api/v1/projects/proj-a/secrets',
    ) as [string, RequestInit]
    expect(secretsGet).toBeTruthy()
    wrapper.unmount()
  })

  it('写/覆写机密：PUT /projects/{name}/secrets/{secret} { value } + 刷新 + 值不回显', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, [project('proj-a')]))
    setRoute('GET', '/api/v1/projects/proj-a/secrets', jsonResponse(200, []))
    setRoute('PUT', '/api/v1/projects/proj-a/secrets/', noContent())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('该项目暂无机密'))

    // 刷新后清单含新机密名。
    setRoute(
      'GET',
      '/api/v1/projects/proj-a/secrets',
      jsonResponse(200, [{ name: 'DEPLOY_KEY' }]),
    )

    await wrapper.get('input[name="secret-name"]').setValue('DEPLOY_KEY')
    await wrapper.get('textarea[name="secret-value"]').setValue('super-secret')
    await wrapper.get('button[name="secret-save"]').trigger('click')

    // 提交形态：PUT /api/v1/projects/proj-a/secrets/DEPLOY_KEY，机密名在路径段、值在体。
    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'PUT',
      ) as [string, RequestInit]
      expect(put[0]).toBe('/api/v1/projects/proj-a/secrets/DEPLOY_KEY')
      expect(JSON.parse(put[1].body as string)).toEqual({ value: 'super-secret' })
    })

    // 成功回执 + 刷新后清单含名；值不回显（无值形态展示）。
    await vi.waitFor(() => expect(wrapper.text()).toContain('机密已写入'))
    await vi.waitFor(() => expect(wrapper.text()).toContain('DEPLOY_KEY'))
    expect(wrapper.text()).not.toContain('super-secret')
    wrapper.unmount()
  })

  it('删除机密：DELETE /projects/{name}/secrets/{secret} + 刷新', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, [project('proj-a')]))
    setRoute(
      'GET',
      '/api/v1/projects/proj-a/secrets',
      jsonResponse(200, [{ name: 'DEPLOY_KEY' }]),
    )
    setRoute('DELETE', '/api/v1/projects/proj-a/secrets/', noContent())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.secret-item').length).toBe(1))

    // 刷新后清单为空。
    setRoute('GET', '/api/v1/projects/proj-a/secrets', jsonResponse(200, []))

    await wrapper.get('button[name="secret-delete"]').trigger('click')

    // 提交形态：DELETE /api/v1/projects/proj-a/secrets/DEPLOY_KEY。
    await vi.waitFor(() => {
      const del = fetchMock.mock.calls.find(
        (c) => (c[1] as RequestInit | undefined)?.method === 'DELETE',
      ) as [string, RequestInit]
      expect(del[0]).toBe('/api/v1/projects/proj-a/secrets/DEPLOY_KEY')
    })
    await vi.waitFor(() => expect(wrapper.text()).toContain('机密已删除'))
    await vi.waitFor(() => expect(wrapper.text()).not.toContain('DEPLOY_KEY'))
    wrapper.unmount()
  })

  it('切换项目 → 重新加载该项目机密', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, [project('proj-a'), project('proj-b')]))
    setRoute('GET', '/api/v1/projects/proj-a/secrets', jsonResponse(200, [{ name: 'A_KEY' }]))
    setRoute('GET', '/api/v1/projects/proj-b/secrets', jsonResponse(200, [{ name: 'B_KEY' }]))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('A_KEY'))
    expect(wrapper.text()).not.toContain('B_KEY')

    await wrapper.get('select[name="secret-project"]').setValue('proj-b')
    await vi.waitFor(() => expect(wrapper.text()).toContain('B_KEY'))
    expect(wrapper.text()).not.toContain('A_KEY')

    // 切换请求命中 proj-b。
    const bGet = fetchMock.mock.calls.find(
      (c) => c[0] === '/api/v1/projects/proj-b/secrets',
    ) as [string, RequestInit]
    expect(bGet).toBeTruthy()
    wrapper.unmount()
  })

  it('422（机密名非法）→ 拼接校验清单就地展示', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, [project('proj-a')]))
    setRoute('GET', '/api/v1/projects/proj-a/secrets', jsonResponse(200, []))
    setRoute(
      'PUT',
      '/api/v1/projects/proj-a/secrets/',
      jsonResponse(422, {
        code: 'VALIDATION_FAILED',
        message: '机密名校验失败',
        detail: {
          errors: [{ path: 'secret', message: '机密名须为非空、由字母数字与下划线组成' }],
        },
      }),
    )

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('该项目暂无机密'))

    await wrapper.get('input[name="secret-name"]').setValue('bad name!')
    await wrapper.get('textarea[name="secret-value"]').setValue('v')
    await wrapper.get('button[name="secret-save"]').trigger('click')

    await vi.waitFor(() =>
      expect(wrapper.get('[role="alert"]').text()).toContain('字母数字与下划线'),
    )
    wrapper.unmount()
  })

  it('无项目 → 空态提示', async () => {
    setRoute('GET', '/api/v1/projects', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))
    expect(wrapper.find('select[name="secret-project"]').exists()).toBe(false)
    wrapper.unmount()
  })
})

// 一级制品库入口（票 #122）：未配置时不可用；管理员仅在已配置时可见测试连接。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import ArtifactRepositoryView from '@/views/ArtifactRepositoryView.vue'
import { i18n, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

const Host = defineComponent({
  name: 'ArtifactRepoHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(ArtifactRepositoryView, { ...attrs }))
  },
})

describe('ArtifactRepositoryView 一级制品库入口（票 #122）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null
  const fetchMock = vi.fn()

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/artifacts', name: 'artifacts', component: { template: '<div />' } }],
    })
    await router.push('/artifacts')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  function mountView(): VueWrapper {
    wrapper = mount(Host, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
  }

  it('未配置 S3 时展示不可用，不显示测试连接', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { available: false, reason: 's3_unconfigured' }),
    )
    useAuthStore().setAuthed({ username: 'admin', isAdmin: true })
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-repo-unavailable"]').exists()).toBe(true))
    expect(w.text()).toContain('制品库不可用')
    expect(w.find('[data-testid="artifact-repo-test"]').exists()).toBe(false)
    const [url] = fetchMock.mock.calls[0] as [string]
    expect(url).toBe('/api/v1/artifact-repository')
  })

  it('已配置时展示后端摘要；普通用户不显示测试连接', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        available: true,
        backend: {
          endpoint: 'https://s3.example.internal:9000',
          region: 'us-east-1',
          bucket: 'sisyphus',
          prefix: 'prod',
          path_style: true,
        },
      }),
    )
    useAuthStore().setAuthed({ username: 'alice', isAdmin: false })
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-repo-available"]').exists()).toBe(true))
    expect(w.text()).toContain('https://s3.example.internal:9000')
    expect(w.text()).toContain('sisyphus')
    expect(w.text()).not.toContain('secret')
    expect(w.find('[data-testid="artifact-repo-test"]').exists()).toBe(false)
  })

  it('管理员测试连接展示逐步结果，响应不含凭据', async () => {
    fetchMock.mockImplementation((url: string) => {
      if (String(url).includes('test-connection')) {
        return Promise.resolve(
          jsonResponse(200, {
            ok: true,
            checks: [
              { op: 'put', ok: true },
              { op: 'head', ok: true },
              { op: 'range_get', ok: true },
              { op: 'copy', ok: true },
              { op: 'multipart_upload', ok: true },
              { op: 'multipart_copy', ok: true },
              { op: 'delete', ok: true },
            ],
          }),
        )
      }
      return Promise.resolve(
        jsonResponse(200, {
          available: true,
          backend: {
            endpoint: 'https://s3.example.internal:9000',
            region: 'us-east-1',
            bucket: 'sisyphus',
            prefix: 'prod',
            path_style: true,
          },
        }),
      )
    })
    useAuthStore().setAuthed({ username: 'admin', isAdmin: true })
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-repo-test"]').exists()).toBe(true))
    await w.get('[data-testid="artifact-repo-test"]').trigger('click')
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-repo-report"]').exists()).toBe(true))
    expect(w.get('[data-testid="artifact-repo-report"]').text()).toContain('put')
    expect(w.get('[data-testid="artifact-repo-report"]').text()).toContain('multipart_copy')
    const testCall = fetchMock.mock.calls.find((c) => String(c[0]).includes('test-connection'))
    expect(testCall?.[0]).toBe('/api/v1/config/s3/test-connection')
    expect(String(JSON.stringify(testCall))).not.toContain('secret')
  })

  it('已启用时展示来源/校验和并按筛选条件重新查询', async () => {
    fetchMock.mockImplementation((url: string) => {
      if (String(url).includes('/artifact-repository/artifacts')) {
        return Promise.resolve(jsonResponse(200, {
          items: [{
            kind: 'set_entry',
            id: 7,
            name: 'dist/app.js',
            set_name: 'bundle',
            path: 'dist/app.js',
            size: 2048,
            sha256: 'a'.repeat(64),
            executable: false,
            backend: 's3',
            availability: 'ready',
            created_at: 1,
            source: { project: 'web-app', pipeline: 'release', build: 12, job: 'package', attempt: 2 },
            download_url: '/api/v1/projects/web-app/pipelines/release/builds/12/artifact-sets/7/file?path=dist%2Fapp.js',
          }],
          total: 1,
          page: 1,
          limit: 50,
          legacy_local_count: 1,
        }))
      }
      return Promise.resolve(jsonResponse(200, {
        available: true,
        backend: { endpoint: 'https://s3.example', region: 'us-east-1', bucket: 'sisyphus', prefix: '', path_style: true },
      }))
    })
    useAuthStore().setAuthed({ username: 'alice', isAdmin: false })
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-browser"]').exists()).toBe(true))
    expect(w.text()).toContain('web-app / release / #12 / package / a2')
    expect(w.text()).toContain('dist/app.js')
    expect(w.find('[data-testid="artifact-repo-legacy"]').exists()).toBe(true)
    await w.get('[data-testid="artifact-filter-project"] input').setValue('web-app')
    await w.get('[data-testid="artifact-browser"] button').trigger('click')
    await vi.waitFor(() => expect(fetchMock.mock.calls.some((call) => String(call[0]).includes('project=web-app'))).toBe(true))
  })

  it('项目管理员可见删除失败与错误，并可将任务显式重新排队', async () => {
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url === '/api/v1/artifact-repository') {
        return jsonResponse(200, {
          available: true,
          backend: { endpoint: 'https://s3.example', region: 'us-east-1', bucket: 'sisyphus', prefix: '', path_style: true },
        })
      }
      if (url === '/api/v1/projects?permission=admin') {
        return jsonResponse(200, [{ id: 1, name: 'demo', scm_type: 'none', scm_url: '', default_branch: null, created_at: 1, updated_at: 1, pipeline_count: 0 }])
      }
      if (url === '/api/v1/projects/demo/artifact-deletions' && (init?.method ?? 'GET') === 'GET') {
        return jsonResponse(200, { items: [{ id: 9, project_id: 1, scope: 'build', state: 'failed', pipeline_name: 'release', build_number: 7, set_id: null, attempts: 2, last_error: 'S3 delete timeout', created_at: 1, updated_at: 2 }] })
      }
      if (url === '/api/v1/projects/demo/artifact-deletions/9/retry' && init?.method === 'POST') {
        return jsonResponse(202, { id: 9, project_id: 1, scope: 'build', state: 'queued', pipeline_name: 'release', build_number: 7, set_id: null, attempts: 2, last_error: null, created_at: 1, updated_at: 3 })
      }
      return jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
    })
    useAuthStore().setAuthed({ username: 'project-admin', isAdmin: false })
    const w = mountView()

    await vi.waitFor(() => expect(w.find('[data-testid="deletion-job-9"]').exists()).toBe(true))
    expect(w.get('[data-testid="deletion-job-9"]').text()).toContain('S3 delete timeout')
    expect(w.get('[data-testid="deletion-job-9"]').text()).toContain('2')
    await w.get('[data-testid="retry-deletion-9"]').trigger('click')
    await vi.waitFor(() => expect(w.get('[data-testid="deletion-job-9"]').text()).toContain('排队中'))
    expect(fetchMock.mock.calls.some((call) => call[0] === '/api/v1/projects/demo/artifact-deletions/9/retry' && call[1]?.method === 'POST')).toBe(true)
  })
})

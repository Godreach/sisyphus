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
})
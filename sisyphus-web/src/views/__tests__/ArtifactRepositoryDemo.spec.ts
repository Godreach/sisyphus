// 制品库 demo 闭环（票 #140）：真实 http client + MSW handler 驱动页面。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import ArtifactRepositoryView from '@/views/ArtifactRepositoryView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'
import { useAuthStore } from '@/stores/auth'

const Host = defineComponent({
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(ArtifactRepositoryView, { ...attrs }))
  },
})

describe('ArtifactRepositoryView demo 删除队列（票 #140）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/artifacts', component: { template: '<div />' } }],
    })
    await router.push('/artifacts')
    await router.isReady()
    server.use(
      http.get('/api/v1/artifact-repository', () => HttpResponse.json({
        available: true,
        backend: { endpoint: 'https://s3.example', region: 'us-east-1', bucket: 'demo', prefix: '', path_style: true },
      })),
      http.get('/api/v1/artifact-repository/artifacts', () => HttpResponse.json({ items: [], total: 0, page: 1, limit: 50, legacy_local_count: 0 })),
      http.get('/api/v1/projects', () => HttpResponse.json([{
        id: 1, name: 'web-app', scm_type: 'git', scm_url: 'https://example.test/web-app.git',
        default_branch: 'main', created_at: 1, updated_at: 1, pipeline_count: 1,
      }])),
    )
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    server.resetHandlers()
  })

  afterAll(() => server.close())

  it('demo 页面展示四种删除状态，失败任务重试后同步为排队中', async () => {
    useAuthStore().setAuthed({ username: 'alice', isAdmin: false })
    wrapper = mount(Host, { global: { plugins: [pinia, router, i18n] } })

    await vi.waitFor(() => expect(wrapper!.find('[data-testid="deletion-panel"]').exists()).toBe(true))
    const panel = wrapper!.get('[data-testid="deletion-panel"]')
    expect(panel.text()).toContain('排队中')
    expect(panel.text()).toContain('删除中')
    expect(panel.text()).toContain('失败')
    expect(panel.text()).toContain('已完成')
    expect(panel.get('[data-testid="deletion-job-103"]').text()).toContain('S3 delete timeout')

    await panel.get('[data-testid="retry-deletion-103"]').trigger('click')
    await vi.waitFor(() => expect(panel.get('[data-testid="deletion-job-103"]').text()).toContain('排队中'))
    expect(panel.get('[data-testid="deletion-job-103"]').text()).not.toContain('S3 delete timeout')
  })
})

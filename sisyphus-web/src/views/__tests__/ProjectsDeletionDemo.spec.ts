// 项目清理状态 demo（票 #139）：组件使用真实 api client + MSW node server，
// 不替换 global fetch，验证外部发起的清理任务可在页面查看状态。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'
import { useAuthStore } from '@/stores/auth'
import ProjectsView from '@/views/ProjectsView.vue'

const PROJECT_NAME = 'page-delete-demo'

describe('ProjectsView 项目清理状态 demo（票 #139）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null
  let deletionListResponses: number

  const Host = defineComponent({
    name: 'ProjectsDeletionDemoHost',
    setup() {
      return () => h(NMessageProvider, () => h(ProjectsView))
    },
  })

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    useAuthStore().setAuthed({ username: 'admin', isAdmin: true })
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects')
    await router.isReady()
    const created = await fetch('/api/v1/projects', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name: PROJECT_NAME,
        scm_type: 'none',
        scm_url: '',
        default_branch: null,
      }),
    })
    expect(created.status).toBe(201)
    deletionListResponses = 0
    server.events.on('response:mocked', ({ request }) => {
      if (request.method === 'GET' && new URL(request.url).pathname === '/api/v1/project-deletions') {
        deletionListResponses += 1
      }
    })
    wrapper = null
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    document.body.innerHTML = ''
    server.resetHandlers()
    server.events.removeAllListeners()
    vi.restoreAllMocks()
  })

  afterAll(() => {
    server.close()
  })

  it('列表卡片无删除入口，清理状态可经队列刷新展示 queued/running/completed', async () => {
    wrapper = mount(Host, {
      attachTo: document.body,
      global: { plugins: [pinia, router, i18n] },
    })
    await vi.waitFor(() => expect(wrapper?.text()).toContain(PROJECT_NAME))
    expect(wrapper.find(`[data-testid="delete-project-${PROJECT_NAME}"]`).exists()).toBe(false)

    const deleted = await fetch(`/api/v1/projects/${PROJECT_NAME}`, { method: 'DELETE' })
    expect(deleted.status).toBe(202)
    const deletion = () => wrapper?.get('li[data-testid^="project-deletion-"]')
    const refresh = () => wrapper?.get('[data-testid="project-deletions"] button')
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(1))
    await vi.waitFor(() => expect(deletion()?.text()).toContain('排队中'))
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(2))
    await vi.waitFor(() => expect(deletion()?.text()).toContain('删除中'))
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(3))
    await vi.waitFor(() => expect(deletion()?.text()).toContain('已完成'))
  })
})

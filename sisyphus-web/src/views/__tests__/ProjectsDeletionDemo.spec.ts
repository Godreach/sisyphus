// 项目删除 demo 闭环（票 #139）：组件使用真实 api client + MSW node server，
// 不替换 global fetch，证明页面流程无需真实 sisyphus-server。

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

describe('ProjectsView 项目删除 demo 闭环（票 #139）', () => {
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

  it('删除后立即隐藏项目，并经队列刷新展示 queued/running/completed', async () => {
    wrapper = mount(Host, {
      attachTo: document.body,
      global: { plugins: [pinia, router, i18n] },
    })
    await vi.waitFor(() => expect(wrapper?.text()).toContain(PROJECT_NAME))

    await wrapper.get(`[data-testid="delete-project-${PROJECT_NAME}"]`).trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
    const actions = document.querySelectorAll('.n-popconfirm__action button')
    await (actions[actions.length - 1] as HTMLElement).click()

    await vi.waitFor(() => expect(
      wrapper?.findAll('.project-card').some((card) => card.text().includes(PROJECT_NAME)),
    ).toBe(false))
    await vi.waitFor(() => expect(wrapper?.find('li[data-testid^="project-deletion-"]').exists()).toBe(true))
    const deletion = () => wrapper?.get('li[data-testid^="project-deletion-"]')
    expect(deletion()?.text()).toContain('排队中')

    const refresh = () => wrapper?.get('[data-testid="project-deletions"] button')
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(1))
    expect(deletion()?.text()).toContain('排队中')
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(2))
    await vi.waitFor(() => expect(deletion()?.text()).toContain('删除中'))
    await refresh()?.trigger('click')
    await vi.waitFor(() => expect(deletionListResponses).toBe(3))
    await vi.waitFor(() => expect(deletion()?.text()).toContain('已完成'))
  })
})

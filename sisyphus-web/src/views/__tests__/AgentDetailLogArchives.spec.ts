import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'
import AgentDetailView from '@/views/AgentDetailView.vue'

describe('AgentDetailView demo 日志归档闭环（票 #142）', () => {
  let router: Router
  let wrapper: VueWrapper | null = null

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/agents', name: 'agents', component: { template: '<div />' } },
        { path: '/agents/:name', name: 'agent-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/agents/build-04')
    await router.isReady()
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('通过真实 API 客户端展示缓冲报告与 pending/lost 积压，不包含 ready 记录', async () => {
    wrapper = mount(AgentDetailView, {
      global: { plugins: [createPinia(), router, i18n] },
    })

    await vi.waitFor(() => expect(wrapper!.text()).toContain('unit-test'))
    expect(wrapper.text()).toContain('lint')
    expect(wrapper.text()).toContain('待归档：Agent 保留日志并在后台重试')
    expect(wrapper.text()).toContain('日志已按独立保留期清理')
    expect(wrapper.text()).toContain('archive retry deferred')
    expect(wrapper.text()).not.toContain('compile')
  })
})

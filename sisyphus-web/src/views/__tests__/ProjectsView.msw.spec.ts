// 项目创建页 demo 闭环（票 #138）：真实 ProjectsView 经 MSW node handlers
// 调用创建期 SCM 探测，验证按钮反馈与默认分支预填，而非手写 fetch stub。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import ProjectsView from '@/views/ProjectsView.vue'
import { i18n, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'
import { server } from '@/mocks/node'

const ProjectsWrapper = defineComponent({
  name: 'ProjectsMswWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(ProjectsView, { ...attrs }))
  },
})

describe('ProjectsView demo MSW SCM 创建闭环（#138）', () => {
  let router: Router
  let wrapper: VueWrapper

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    setActivePinia(createPinia())
    useAuthStore().setAuthed({ username: 'admin', isAdmin: true })
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/projects', name: 'projects', component: { template: '<div />' } }],
    })
    await router.push({ path: '/projects', query: { create: '1' } })
    await router.isReady()
  })

  afterEach(() => {
    wrapper?.unmount()
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('demo 中点击测试连接展示成功 head，并预填 Git 默认分支', async () => {
    wrapper = mount(ProjectsWrapper, { global: { plugins: [router, i18n] } })
    await vi.waitFor(() => expect(wrapper.find('input[name="project-url"]').exists()).toBe(true))

    await wrapper.get('input[name="project-url"]').setValue('https://example.com/demo.git')
    await wrapper.get('button[name="project-test-connection"]').trigger('click')

    await vi.waitFor(() => expect(wrapper.text()).toContain('连接成功，当前 head：abc123deadbeef'))
    expect((wrapper.get('input[name="project-branch"]').element as HTMLInputElement).value).toBe('main')
  })
})

// 应用壳登出动作测试（B4-T2 登出闭环）：已登录用户底部可见用户名 + 登出
// 按钮；点击登出调 POST /auth/logout（删会话 + 清 cookie）并回登录页。
// 只测外部行为（DOM 事件 + 请求形态），API 层以 fetch mock 驱动。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import App from '@/App.vue'
import { i18n, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'

describe('App 壳（登出闭环）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  const fetchMock = vi.fn()

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/login', name: 'login', component: { template: '<div />' } },
        { path: '/', name: 'overview', component: { template: '<div />' } },
      ],
    })
    await router.push('/')
    await router.isReady()
    globalThis.fetch = fetchMock
    wrapper = mount(App, {
      global: {
        plugins: [pinia, router, i18n],
      },
    })
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('未登录不显示登出按钮', () => {
    expect(wrapper.find('button.footer-logout').exists()).toBe(false)
  })

  it('已登录显示用户名 + 登出按钮；点击登出调 POST /auth/logout 并回登录页', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })

    await wrapper.vm.$nextTick()
    expect(wrapper.text()).toContain('alice')

    const replaceSpy = vi.spyOn(router, 'replace')
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))

    await wrapper.get('button.footer-logout').trigger('click')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith({ name: 'login' }))

    // 请求形态：POST /api/v1/auth/logout（登出删会话 + 清 cookie）。
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/auth/logout')
    expect(init.method).toBe('POST')

    // 登出后认证态已清。
    expect(auth.isAuthed).toBe(false)
    expect(auth.user).toBeNull()
  })
})

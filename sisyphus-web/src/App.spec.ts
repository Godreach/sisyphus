// 应用壳登出动作测试（B4-T2 登出闭环）：已登录用户底部可见用户名 + 登出
// 按钮；点击登出调 POST /auth/logout（删会话 + 清 cookie）并回登录页。
// 只测外部行为（DOM 事件 + 请求形态），API 层以 fetch mock 驱动。
// #87: Footer 改用 Naive UI 组件（NButton/NText/NSwitch），窄屏侧栏折叠为 NDrawer。

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
    expect(wrapper.find('.footer-user').exists()).toBe(false)
  })

  it('已登录显示用户名 + 登出按钮；点击登出调 POST /auth/logout 并回登录页', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })

    await wrapper.vm.$nextTick()
    expect(wrapper.text()).toContain('alice')

    const replaceSpy = vi.spyOn(router, 'replace')
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))

    // NButton 渲染为 <button class="n-button">，footer-user 区域内的按钮即登出。
    const logoutBtn = wrapper.find('.footer-user button')
    await logoutBtn.trigger('click')
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

describe('App 壳（管理区侧栏 is_admin 门控）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/admin/secrets', name: 'admin-secrets', component: { template: '<div />' } },
        { path: '/admin/audit', name: 'admin-audit', component: { template: '<div />' } },
        { path: '/admin/upgrade', name: 'admin-upgrade', component: { template: '<div />' } },
        { path: '/admin/users', name: 'admin-users', component: { template: '<div />' } },
      ],
    })
    await router.push('/')
    await router.isReady()
    globalThis.fetch = vi.fn()
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('全局 admin 侧栏显示管理区四入口 + 管理分组标题', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'admin', isAdmin: true })
    await wrapper.vm.$nextTick()

    const sidebarText = wrapper.find('.app-sidebar').text()
    expect(sidebarText).toContain('管理')
    expect(sidebarText).toContain('机密')
    expect(sidebarText).toContain('审计日志')
    expect(sidebarText).toContain('Agent 升级')
    expect(sidebarText).toContain('用户')
  })

  it('非全局 admin 侧栏不显示管理区入口（无管理分组）', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    await wrapper.vm.$nextTick()

    const sidebarText = wrapper.find('.app-sidebar').text()
    // 主区仍在。
    expect(sidebarText).toContain('主区')
    expect(sidebarText).toContain('概览')
    expect(sidebarText).toContain('项目')
    // 管理区不可见。
    expect(sidebarText).not.toContain('管理')
    expect(sidebarText).not.toContain('机密')
    expect(sidebarText).not.toContain('审计日志')
  })
})

describe('App 壳（窄屏响应式 #87）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper
  let mediaQuery: MockMediaQueryList

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
      ],
    })
    await router.push('/')
    await router.isReady()
    globalThis.fetch = vi.fn()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('桌面端（≥768px）显示侧栏，不显示汉堡按钮', async () => {
    mediaQuery = createMockMediaQuery(false) // isNarrow = false
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    await wrapper.vm.$nextTick()

    expect(wrapper.find('.app-sidebar').exists()).toBe(true)
    expect(wrapper.find('.app-topbar').exists()).toBe(false)
  })

  it('窄屏（<768px）隐藏侧栏，显示汉堡按钮', async () => {
    mediaQuery = createMockMediaQuery(true) // isNarrow = true
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    await wrapper.vm.$nextTick()

    expect(wrapper.find('.app-sidebar').exists()).toBe(false)
    expect(wrapper.find('.app-topbar').exists()).toBe(true)
  })

  it('窄屏点击汉堡按钮打开 Drawer', async () => {
    mediaQuery = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    await wrapper.vm.$nextTick()

    // Drawer 初始关闭（teleport 到 body）。
    expect(document.body.querySelector('.n-drawer')).toBeNull()

    // 点击汉堡按钮。
    await wrapper.find('.app-topbar button').trigger('click')
    await wrapper.vm.$nextTick()
    await vi.waitFor(() => {
      expect(document.body.querySelector('.n-drawer')).not.toBeNull()
    })
  })
})

// ---------- helpers ----------

interface MockMediaQueryList {
  matches: boolean
  addEventListener: ReturnType<typeof vi.fn>
  removeEventListener: ReturnType<typeof vi.fn>
  _simulateChange: (matches: boolean) => void
}

function createMockMediaQuery(matches: boolean): MockMediaQueryList {
  let _handler: ((e: MediaQueryListEvent) => void) | null = null
  return {
    matches,
    addEventListener: vi.fn((_type: string, handler: (e: MediaQueryListEvent) => void) => {
      _handler = handler
    }),
    removeEventListener: vi.fn(),
    _simulateChange(newMatches: boolean) {
      this.matches = newMatches
      _handler?.({ matches: newMatches } as MediaQueryListEvent)
    },
  }
}

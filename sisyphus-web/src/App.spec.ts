// 应用壳测试（spec #99 重构后的外壳行为）：
// - 深侧栏严格三项导航（工作台/流水线/构建机）；未认证无壳。
// - 登出闭环：用户卡下拉 → 登出 → POST /auth/logout + 回登录页 + 清认证态。
// - 管理四页入口收编进用户卡下拉：仅全局 admin 可见；非 admin 侧栏与
//   下拉均无管理入口。直访 URL 由路由守卫兜底（guards.spec.ts 覆盖）。
// - 窄屏（<768px）侧栏折叠为汉堡 + NDrawer（#87 行为保留）。
// 只测外部行为（DOM 事件 + 请求形态），API 层以 fetch mock 驱动。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import App from '@/App.vue'
import { i18n, setLocale } from '@/i18n'
import { useAuthStore } from '@/stores/auth'

describe('App 壳（三项导航 + 登出闭环）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  const fetchMock = vi.fn()

  beforeEach(async () => {
    setLocale('zh-CN')
    localStorage.removeItem('sisyphus-sidebar-width')
    localStorage.removeItem('sisyphus-theme')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/login', name: 'login', component: { template: '<div />' } },
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/pipelines', name: 'pipelines', component: { template: '<div />' } },
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

  it('未认证不渲染侧栏（无壳布局）', () => {
    expect(wrapper.find('.app-sidebar').exists()).toBe(false)
    expect(wrapper.find('[data-testid="sidebar-user"]').exists()).toBe(false)
  })

  it('已登录侧栏严格三项导航；点击导航跳路由', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    await wrapper.vm.$nextTick()

    const items = wrapper.findAll('.app-sidebar .nav-item')
    expect(items.map((w) => w.text())).toEqual(['工作台', '流水线', '构建机'])

    const pushSpy = vi.spyOn(router, 'push')
    await items[1]?.trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({ name: 'pipelines' })
  })

  it('侧栏宽度可拖拽调整并持久化；双击复位', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    await wrapper.vm.$nextTick()

    const resizer = wrapper.find('.sidebar-resizer')
    expect(resizer.exists()).toBe(true)
    const sidebar = wrapper.find('.app-sidebar')

    // 拖拽：pointerdown(232) → move(+80) → up → 宽度 312 + 写入 localStorage。
    await resizer.trigger('pointerdown', { clientX: 232 })
    window.dispatchEvent(new MouseEvent('pointermove', { clientX: 312 }))
    window.dispatchEvent(new MouseEvent('pointerup'))
    await wrapper.vm.$nextTick()

    expect(sidebar.attributes('style')).toContain('312px')
    expect(localStorage.getItem('sisyphus-sidebar-width')).toBe('312')

    // 钳制：拖到 600px 被上限 400 截住。
    await resizer.trigger('pointerdown', { clientX: 312 })
    window.dispatchEvent(new MouseEvent('pointermove', { clientX: 600 }))
    window.dispatchEvent(new MouseEvent('pointerup'))
    await wrapper.vm.$nextTick()
    expect(sidebar.attributes('style')).toContain('400px')

    // 双击复位默认 232。
    await resizer.trigger('dblclick')
    await wrapper.vm.$nextTick()
    expect(sidebar.attributes('style')).toContain('232px')
    expect(localStorage.getItem('sisyphus-sidebar-width')).toBe('232')
  })

  it('用户卡显示用户名；下拉登出调 POST /auth/logout 并回登录页', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })

    await wrapper.vm.$nextTick()
    expect(wrapper.find('[data-testid="sidebar-user"]').text()).toContain('alice')

    const replaceSpy = vi.spyOn(router, 'replace')
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }))

    // 打开用户卡下拉（teleport 到 body），点「登出」项。
    // naive-ui 的点击绑定在 .n-dropdown-option-body 上。
    await wrapper.find('[data-testid="sidebar-user"]').trigger('click')
    await vi.waitFor(() => {
      expect(document.body.querySelector('.n-dropdown')).not.toBeNull()
    })
    const logoutBody = [...document.body.querySelectorAll('.n-dropdown-option-body')].find((el) =>
      el.textContent?.includes('登出'),
    )
    expect(logoutBody).toBeDefined()
    logoutBody!.dispatchEvent(new MouseEvent('click', { bubbles: true }))
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

// 票 #112：setup 引导在第一步建管理员后即登录（isAuthed 翻 true），但向导
// 仍未走完（停在 Agent/项目步）。若此时 App.vue 切到 app-shell，向导第二步起
// 会冒出深侧栏，破坏「全屏无侧栏形态」首装体验。已登录直访 /login 或 /setup
// 由路由守卫回首页——此处仅兜住向导中途的登录态，认证面恒为无壳居中。
describe('App 壳（认证面恒无壳，票 #112）', () => {
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
        { path: '/setup', name: 'setup', component: { template: '<div />' } },
        { path: '/login', name: 'login', component: { template: '<div />' } },
        { path: '/', name: 'overview', component: { template: '<div />' } },
      ],
    })
    await router.push('/setup')
    await router.isReady()
    globalThis.fetch = vi.fn()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('setup 引导中途登录（isAuthed 翻 true）仍无侧栏：向导全程全屏无侧栏', async () => {
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    const auth = useAuthStore()
    // 向导第一步建管理员后 auth.login 置 authed——模拟此中途态。
    auth.setAuthed({ username: 'admin', isAdmin: true })
    await wrapper.vm.$nextTick()

    expect(auth.isAuthed).toBe(true)
    // 认证面（login/setup）即便已登录也走无壳居中布局。
    expect(wrapper.find('.app-sidebar').exists()).toBe(false)
    expect(wrapper.find('.app-bare').exists()).toBe(true)
  })

  it('已登录访问 /login 同样无壳（守卫已回首页，此处兜底不冒侧栏）', async () => {
    await router.push('/login')
    await router.isReady()
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    const auth = useAuthStore()
    auth.setAuthed({ username: 'admin', isAdmin: true })
    await wrapper.vm.$nextTick()

    expect(wrapper.find('.app-sidebar').exists()).toBe(false)
  })
})

describe('App 壳（管理入口收编进用户卡下拉）', () => {
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

  /** 打开用户卡下拉并返回 body 内选项文本。 */
  async function openUserMenu(): Promise<string> {
    await wrapper.find('[data-testid="sidebar-user"]').trigger('click')
    await vi.waitFor(() => {
      expect(document.body.querySelector('.n-dropdown')).not.toBeNull()
    })
    return (
      [...document.body.querySelectorAll('.n-dropdown-option')]
        .map((el) => el.textContent ?? '')
        .join('|') ?? ''
    )
  }

  it('全局 admin：用户卡下拉含管理四页入口', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'admin', isAdmin: true })
    await wrapper.vm.$nextTick()

    const menuText = await openUserMenu()
    expect(menuText).toContain('机密')
    expect(menuText).toContain('审计日志')
    expect(menuText).toContain('构建机升级')
    expect(menuText).toContain('用户')
    // 侧栏本体严格三项（无管理分组）。
    expect(wrapper.find('.app-sidebar').text()).not.toContain('机密')
    expect(wrapper.find('.app-sidebar').text()).not.toContain('审计日志')
  })

  it('非全局 admin：侧栏无管理入口，下拉只有登出', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    await wrapper.vm.$nextTick()

    const sidebarText = wrapper.find('.app-sidebar').text()
    expect(sidebarText).toContain('工作台')
    expect(sidebarText).toContain('流水线')
    expect(sidebarText).toContain('构建机')
    expect(sidebarText).not.toContain('机密')
    expect(sidebarText).not.toContain('审计日志')

    const menuText = await openUserMenu()
    expect(menuText).toContain('登出')
    expect(menuText).not.toContain('机密')
    expect(menuText).not.toContain('审计日志')
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
    expect(wrapper.find('.app-topbar button').exists()).toBe(false)
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

describe('App 壳（语言/主题收进用户卡菜单，票 #104 裁定 G3/G4）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  beforeEach(async () => {
    setLocale('zh-CN')
    localStorage.removeItem('sisyphus-theme')
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

  function bodyOptionBodies(): HTMLElement[] {
    return [...document.body.querySelectorAll('.n-dropdown-option-body')] as HTMLElement[]
  }

  /** 打开用户卡下拉。 */
  async function openUserMenu(): Promise<void> {
    await wrapper.find('[data-testid="sidebar-user"]').trigger('click')
    await vi.waitFor(() => {
      expect(document.body.querySelector('.n-dropdown')).not.toBeNull()
    })
  }

  it('顶栏无语言开关；用户卡下拉有「语言」「主题」二级菜单父项', async () => {
    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    await wrapper.vm.$nextTick()

    // G3：顶栏「中/EN」开关移除。
    expect(wrapper.find('.n-switch').exists()).toBe(false)

    await openUserMenu()
    // 二级菜单父项常驻（叶子项悬停展开，为 naive-ui 库内行为，叶子
    // 结构/切换分发在 utils/userMenu.spec 验证）。
    const labels = bodyOptionBodies().map((el) => el.textContent ?? '')
    expect(labels.some((s) => s?.includes('语言'))).toBe(true)
    expect(labels.some((s) => s?.includes('主题'))).toBe(true)
    expect(labels.some((s) => s?.includes('登出'))).toBe(true)
  })

  it('窄屏抽屉：偏好下拉同源（语言/主题不丢入口）', async () => {
    const mq = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mq as unknown as MediaQueryList)
    wrapper.unmount()

    const auth = useAuthStore()
    auth.setAuthed({ username: 'alice', isAdmin: false })
    wrapper = mount(App, { global: { plugins: [pinia, router, i18n] } })
    await wrapper.vm.$nextTick()

    // 汉堡打开抽屉 → 抽屉偏好入口打开同源下拉。
    await wrapper.find('.app-topbar button').trigger('click')
    await vi.waitFor(() => {
      expect(document.body.querySelector('.n-drawer')).not.toBeNull()
    })
    const prefsBtn = document.body.querySelector('[data-testid="drawer-prefs"]') as HTMLElement
    expect(prefsBtn).not.toBeNull()
    prefsBtn.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await vi.waitFor(() => {
      const labels = [...document.body.querySelectorAll('.n-dropdown-option-body')].map((el) =>
        el.textContent,
      )
      expect(labels.some((s) => s?.includes('语言'))).toBe(true)
      expect(labels.some((s) => s?.includes('主题'))).toBe(true)
      expect(labels.some((s) => s?.includes('登出'))).toBe(true)
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

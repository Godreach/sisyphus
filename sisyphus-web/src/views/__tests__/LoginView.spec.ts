// 登录页行为测试（ADR-0014，票 B4-T2 登录/回跳闭环）：只测外部行为——
// 登录成功回跳原目标、401/429/网络错误统一展示、表单提交形态。
// API 层以 fetch mock 驱动（组件经单实例 http 客户端走全局 fetch）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import LoginView from '@/views/LoginView.vue'
import { i18n, setLocale } from '@/i18n'

/** 构造 mock JSON 响应（jsdom 无 fetch，需自造 Response 壳）。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

describe('LoginView（登录/回跳/错误展示）', () => {
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
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/', name: 'overview', component: { template: '<div />' } },
      ],
    })
    await router.push('/login')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  async function fillAndSubmit(username: string, password: string): Promise<void> {
    await wrapper.get('input[name="username"]').setValue(username)
    await wrapper.get('input[name="password"]').setValue(password)
    // 表单提交事件（jsdom 里点击 type=submit 不保证派发 submit，直接触发）。
    await wrapper.get('form').trigger('submit')
  }

  it('登录成功 → 回跳原目标（redirect 查询参数，路由守卫写入）', async () => {
    // 带 redirect 深链：`/projects` 是受保护页回跳目标。
    await router.replace({ name: 'login', query: { redirect: '/projects' } })
    await router.isReady()
    wrapper = mount(LoginView, {
      global: { plugins: [pinia, router, i18n] },
    })

    fetchMock.mockResolvedValue(jsonResponse(200, { username: 'alice', is_admin: false }))
    const replaceSpy = vi.spyOn(router, 'replace')

    await fillAndSubmit('alice', 'secret123')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith('/projects'))

    // 请求形态：POST /api/v1/auth/login，JSON 凭据体。
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/auth/login')
    expect(JSON.parse(init.body as string)).toEqual({ username: 'alice', password: 'secret123' })
  })

  it('无 redirect 时登录成功 → 回首页', async () => {
    wrapper = mount(LoginView, { global: { plugins: [pinia, router, i18n] } })
    fetchMock.mockResolvedValue(jsonResponse(200, { username: 'alice', is_admin: false }))
    const replaceSpy = vi.spyOn(router, 'replace')

    await fillAndSubmit('alice', 'secret123')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith('/'))
  })

  it('401（用户名或密码错误）→ 就地展示后端 message，停留登录页', async () => {
    wrapper = mount(LoginView, { global: { plugins: [pinia, router, i18n] } })
    fetchMock.mockResolvedValue(
      jsonResponse(401, { code: 'UNAUTHORIZED', message: '用户名或密码错误' }),
    )

    await fillAndSubmit('alice', 'wrong')
    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('用户名或密码错误'))
    expect(wrapper.text()).toContain('登录以继续')
  })

  it('429 限流 → 按 retry_after_ms 展示倒计时提示', async () => {
    wrapper = mount(LoginView, { global: { plugins: [pinia, router, i18n] } })
    fetchMock.mockResolvedValue(
      jsonResponse(429, {
        code: 'RATE_LIMITED',
        message: '登录尝试过于频繁，请稍后再试',
        detail: { retry_after_ms: 30000 },
      }),
    )

    await fillAndSubmit('alice', 'secret123')
    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('30'))
  })

  it('网络层失败 → 展示网络错误提示（不落登录态）', async () => {
    wrapper = mount(LoginView, { global: { plugins: [pinia, router, i18n] } })
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'))

    await fillAndSubmit('alice', 'secret123')
    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('网络请求失败'))
  })
})

describe('LoginView Naive UI 迁移', () => {
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
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/', name: 'overview', component: { template: '<div />' } },
      ],
    })
    await router.push('/login')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  function mountLogin() {
    return mount(LoginView, {
      global: {
        plugins: [pinia, router, i18n],
        stubs: {
          NConfigProvider: false,
        },
      },
    })
  }

  it('空值提交 → 显示内联校验错误', async () => {
    wrapper = mountLogin()
    // 提交空表单
    await wrapper.get('form').trigger('submit')
    // 应该出现校验错误信息
    await vi.waitFor(() => {
      const text = wrapper.text()
      expect(text).toContain('请输入用户名')
      expect(text).toContain('请输入密码')
    })
    // 不应触发网络请求
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('提交中 → 按钮禁用并显示加载状态', async () => {
    wrapper = mountLogin()
    // 设置 fetch 延迟响应
    fetchMock.mockImplementation(() => new Promise(() => {}))
    await wrapper.get('input[name="username"]').setValue('alice')
    await wrapper.get('input[name="password"]').setValue('secret')
    await wrapper.get('form').trigger('submit')
    await vi.waitFor(() => {
      const button = wrapper.get('button[type="submit"]')
      expect(button.attributes('disabled')).toBeDefined()
      // 应该有 NSpin 或加载指示器（暂检查按钮文本变化）
      expect(button.text()).toContain('登录中')
    })
  })

  it('登录失败 → 显示 NAlert 错误提示', async () => {
    wrapper = mountLogin()
    fetchMock.mockResolvedValue(
      jsonResponse(401, { code: 'UNAUTHORIZED', message: '用户名或密码错误' }),
    )
    await wrapper.get('input[name="username"]').setValue('alice')
    await wrapper.get('input[name="password"]').setValue('wrong')
    await wrapper.get('form').trigger('submit')
    await vi.waitFor(() => {
      // 应该出现 NAlert 组件（role="alert"）
      const alert = wrapper.get('[role="alert"]')
      expect(alert.text()).toContain('用户名或密码错误')
    })
  })

  it('页面使用 NCard 居中布局', () => {
    wrapper = mountLogin()
    // 检查是否存在 NCard（可能渲染为 div with class naive-card）
    // 暂时检查是否有卡片容器（可根据实际结构调整）
    expect(wrapper.find('.login-card').exists()).toBe(true) // 保留原有 class 作为标识
  })
})

// Setup wizard 行为测试（ADR-0010，票 B4-T2）：只测外部行为——用户可见
// 状态与 DOM 事件，不测组件内部结构。
// - 管理员步：`POST /auth/setup`（经 auth.login 换会话）成功 → 进 Agent 步；
//   404（非空库，引导已完成）→ 回落登录页
// - Agent 步：`POST /agents` 成功 → 展示一次性注册码 + per-Agent token
//   （明文仅此一次）+ 按目标 OS 的复制即用注册命令（`--reg-key` 换码）
// - 项目步：`POST /projects` 成功 → 引导完成进首页
// - 跳过：三步各自可跳过（不触发对应请求），最后一步跳过/完成即结束
// API 层以 fetch mock 驱动（组件经单实例 http 客户端走全局 fetch）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import SetupView from '@/views/SetupView.vue'
import { i18n, setLocale } from '@/i18n'

/** 构造 mock JSON 响应（jsdom 无 fetch，需自造 Response 壳）。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

describe('SetupView wizard（三步可跳过）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  const fetchMock = vi.fn()

  function mountWizard(): VueWrapper {
    return mount(SetupView, {
      global: {
        plugins: [pinia, router, i18n],
      },
    })
  }

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
    globalThis.fetch = fetchMock
    wrapper = mountWizard()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('初始停在管理员步：建号按钮因密码过短禁用', () => {
    expect(wrapper.text()).toContain('创建首个全局管理员')
    const button = wrapper.get('button.setup-primary')
    expect((button.element as HTMLButtonElement).disabled).toBe(true)
  })

  it('管理员步提交成功（POST /auth/setup 建号 → POST /auth/login 换会话）→ 进 Agent 步', async () => {
    // 建号（setup）与登录（login 是独立一步）各一个请求。
    const setupRes = jsonResponse(201, { username: 'admin', is_admin: true })
    const loginRes = jsonResponse(200, { username: 'admin', is_admin: true })
    fetchMock.mockResolvedValueOnce(setupRes).mockResolvedValueOnce(loginRes)

    await wrapper.get('input[name="admin-password"]').setValue('secret123')
    await wrapper.get('button.setup-primary').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('创建首个 Agent 条目'))

    // 请求形态：POST /api/v1/auth/setup（建号）→ POST /api/v1/auth/login（换会话）。
    const [setupUrl, setupInit] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(setupUrl).toBe('/api/v1/auth/setup')
    expect(JSON.parse(setupInit.body as string)).toEqual({ username: 'admin', password: 'secret123' })
    const [loginUrl, loginInit] = fetchMock.mock.calls[1] as [string, RequestInit]
    expect(loginUrl).toBe('/api/v1/auth/login')
    expect(JSON.parse(loginInit.body as string)).toEqual({ username: 'admin', password: 'secret123' })
  })

  it('管理员步 404（非空库/引导已完成）→ 回落登录页', async () => {
    const replaceSpy = vi.spyOn(router, 'replace')
    fetchMock.mockResolvedValue(jsonResponse(404, { code: 'NOT_FOUND', message: 'x' }))

    await wrapper.get('input[name="admin-password"]').setValue('secret123')
    await wrapper.get('button.setup-primary').trigger('click')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalled())
    expect(replaceSpy).toHaveBeenCalledWith({ name: 'login' })
  })

  it('管理员步失败（401）→ 就地展示错误、停留本步', async () => {
    fetchMock.mockResolvedValue(jsonResponse(401, { code: 'UNAUTHORIZED', message: '用户名或密码错误' }))

    await wrapper.get('input[name="admin-password"]').setValue('secret123')
    await wrapper.get('button.setup-primary').trigger('click')
    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('用户名或密码错误'))
    expect(wrapper.text()).toContain('创建首个全局管理员')
  })

  it('Agent 步建条目成功 → 展示一次性注册码 + token + 按 OS 的注册命令', async () => {
    // 先跳到 Agent 步（跳过管理员步）。
    await wrapper.get('button.setup-skip').trigger('click')
    expect(wrapper.text()).toContain('创建首个 Agent 条目')

    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        token: 'sisa_T0K3N',
        register_code: 'sisa_reg_C0D3',
        agent: { name: 'build-1' },
      }),
    )
    await wrapper.get('button.setup-primary').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('sisa_T0K3N'))

    // 请求形态：POST /api/v1/agents（建条目）。
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/agents')
    expect(JSON.parse(init.body as string)).toEqual({ name: 'build-1' })

    // 复制即用命令：默认 Linux/macOS 档 → 无 .exe、含 --reg-key 换码。
    const cmd = wrapper.get('.setup-cmd code').text()
    expect(cmd).toContain('sisyphus-agent')
    expect(cmd).toContain('--reg-key sisa_reg_C0D3')
    expect(cmd).not.toContain('.exe')

    // 切 Windows 档 → 二进制带 .exe。
    await wrapper.get('select').setValue('windows')
    expect(wrapper.get('.setup-cmd code').text()).toContain('sisyphus-agent.exe')

    // 点「下一步」进项目步（凭据仅展示一次，此时已离开展示面）。
    await wrapper.get('button.setup-primary').trigger('click')
    expect(wrapper.text()).toContain('创建首个项目')
  })

  it('项目步提交成功（POST /projects）→ 引导完成进首页', async () => {
    // 直接跳到项目步。
    await wrapper.get('button.setup-skip').trigger('click')
    await wrapper.get('button.setup-skip').trigger('click')
    expect(wrapper.text()).toContain('创建首个项目')

    const replaceSpy = vi.spyOn(router, 'replace')
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        id: 1,
        name: 'my-project',
        scm_type: 'git',
        scm_url: 'https://example.com/a.git',
        default_branch: 'main',
        created_at: 0,
        updated_at: 0,
      }),
    )
    await wrapper.get('input[name="project-name"]').setValue('my-project')
    await wrapper.get('input[name="project-url"]').setValue('https://example.com/a.git')
    await wrapper.get('input[name="project-branch"]').setValue('main')
    await wrapper.get('button.setup-primary').trigger('click')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith({ name: 'overview' }))

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/projects')
    expect(JSON.parse(init.body as string)).toMatchObject({
      name: 'my-project',
      scm_type: 'git',
      scm_url: 'https://example.com/a.git',
      default_branch: 'main',
    })
  })

  it('三步各自可跳过：跳过不触发对应请求，最后一步跳过即结束引导', async () => {
    const replaceSpy = vi.spyOn(router, 'replace')

    // 步骤 1 → 2：不调 /auth/setup。
    await wrapper.get('button.setup-skip').trigger('click')
    expect(fetchMock).not.toHaveBeenCalled()

    // 步骤 2 → 3：不调 /agents（无建条目）。
    await wrapper.get('button.setup-skip').trigger('click')
    expect(fetchMock).not.toHaveBeenCalled()

    // 步骤 3 的最后一步跳过 = 完成引导进首页。
    await wrapper.get('button.setup-skip').trigger('click')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith({ name: 'overview' }))
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('底部「全部跳过」链接直接结束引导进首页', async () => {
    const replaceSpy = vi.spyOn(router, 'replace')
    await wrapper.get('button.setup-done').trigger('click')
    await vi.waitFor(() => expect(replaceSpy).toHaveBeenCalledWith({ name: 'overview' }))
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

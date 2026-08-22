// 项目列表 + 新建行为测试（ADR-0016/0020，票 B4-T3）：只测外部行为。
// - 列表：GET /projects（可见性过滤），点击进项目详情
// - 新建：POST /projects（git/svn + 仓库 URL + git 默认分支），成功收表单
//   并刷新列表；403（非全局 admin）就地展示
// - 测试连接不阻塞保存：端点未交付 → 按钮禁用 + 提示态，保存不依赖该动作
// - ls-remote 预填端点未交付 → 分支字段手动输入 + 提示
// 视图在 onMounted 即发列表请求：mount 须在设置 fetch mock 之后。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import ProjectsView from '@/views/ProjectsView.vue'
import { i18n, setLocale } from '@/i18n'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

function project(id: number, name: string, scmType: 'git' | 'svn', url: string) {
  return {
    id,
    name,
    scm_type: scmType,
    scm_url: url,
    default_branch: scmType === 'git' ? 'main' : null,
    created_at: 0,
    updated_at: 0,
  }
}

describe('ProjectsView 项目列表 + 新建', () => {
  let pinia: Pinia
  let router: Router

  const fetchMock = vi.fn()

  /** 包装组件：NMessageProvider + ProjectsView，保证 useMessage 注入可用。 */
  const ProjectsWrapper = defineComponent({
    name: 'ProjectsWrapper',
    setup(_, { attrs }) {
      return () => h(NMessageProvider, () => h(ProjectsView, { ...attrs }))
    },
  })

  function mountView(): VueWrapper {
    return mount(ProjectsWrapper, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载后列出项目（GET /projects），点击进项目详情', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, [project(1, 'demo', 'git', 'https://x/a.git')]))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('demo'))

    const pushSpy = vi.spyOn(router, 'push')
    // 卡片布局：点击项目卡（NCard 整体可点）进详情。
    await wrapper.get('.project-card').trigger('click')
    expect(pushSpy).toHaveBeenCalledWith({ name: 'project-detail', params: { name: 'demo' } })

    const [url] = fetchMock.mock.calls[0] as [string]
    expect(url).toBe('/api/v1/projects')
    wrapper.unmount()
  })

  it('无项目：展示空态提示', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))
    wrapper.unmount()
  })

  it('新建项目（git + 默认分支）：POST /projects 成功 → 收表单 + 刷新列表', async () => {
    const routes = new Map<string, Response>()
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const method = init?.method ?? 'GET'
      if (url === '/api/v1/projects' && method === 'GET') {
        return routes.get('list') ?? jsonResponse(200, [])
      }
      if (url === '/api/v1/projects' && method === 'POST') {
        return routes.get('create') ?? jsonResponse(201, project(3, 'newproj', 'git', 'https://x/new.git'))
      }
      return jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
    })
    routes.set('list', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))

    // 展开新建表单。
    await wrapper.get('button[name="project-new"]').trigger('click')
    expect(wrapper.text()).toContain('仓库类型')

    routes.set('create', jsonResponse(201, project(3, 'newproj', 'git', 'https://x/new.git')))
    await wrapper.get('input[name="project-name"]').setValue('newproj')
    await wrapper.get('input[name="project-url"]').setValue('https://x/new.git')
    await wrapper.get('input[name="project-branch"]').setValue('release')
    await wrapper.get('button[name="project-save"]').trigger('click')

    // 提交形态：POST /api/v1/projects，git 带 default_branch。
    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find((c) => (c as [string, RequestInit])[1]?.method === 'POST')
      expect(post).toBeTruthy()
      const [url, init] = post as unknown as [string, RequestInit]
      expect(url).toBe('/api/v1/projects')
      expect(JSON.parse(init.body as string)).toMatchObject({
        name: 'newproj',
        scm_type: 'git',
        scm_url: 'https://x/new.git',
        default_branch: 'release',
      })
    })
    // 成功即刷新列表（再次 GET /projects）。
    await vi.waitFor(() => {
      expect(fetchMock.mock.calls.filter((c) => (c as [string])[0] === '/api/v1/projects')).toHaveLength(3)
    })
    wrapper.unmount()
  })

  it('svn 项目：不显示 git 默认分支字段，提交不带 default_branch', async () => {
    const routes = new Map<string, Response>()
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const method = init?.method ?? 'GET'
      if (url === '/api/v1/projects' && method === 'GET') {
        return routes.get('list') ?? jsonResponse(200, [])
      }
      if (url === '/api/v1/projects' && method === 'POST') {
        return routes.get('create') ?? jsonResponse(201, project(4, 'legacy', 'svn', 'https://svn/x/trunk'))
      }
      return jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
    })
    routes.set('list', jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))

    await wrapper.get('button[name="project-new"]').trigger('click')
    // 选 svn（NSelect 下拉：点开菜单再点选项，jsdom 下需 virtual-scroll 关闭）。
    await wrapper.get('.n-base-selection').trigger('click')
    await vi.waitFor(() => {
      const svnOption = [...document.querySelectorAll('.n-base-select-option')].find((o) => o.textContent === 'svn')
      expect(svnOption).toBeTruthy()
      ;(svnOption as HTMLElement).click()
    })
    await new Promise((r) => setTimeout(r, 50))
    expect(wrapper.find('input[name="project-branch"]').exists()).toBe(false)

    routes.set('create', jsonResponse(201, project(4, 'legacy', 'svn', 'https://svn/x/trunk')))
    await wrapper.get('input[name="project-name"]').setValue('legacy')
    await wrapper.get('input[name="project-url"]').setValue('https://svn/x/trunk')
    await wrapper.get('button[name="project-save"]').trigger('click')

    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find((c) => (c as [string, RequestInit])[1]?.method === 'POST')
      expect(post).toBeTruthy()
      const [url, init] = post as unknown as [string, RequestInit]
      expect(url).toBe('/api/v1/projects')
      expect(JSON.parse(init.body as string)).toMatchObject({
        name: 'legacy',
        scm_type: 'svn',
        scm_url: 'https://svn/x/trunk',
        default_branch: null,
      })
    })
    wrapper.unmount()
  })

  it('测试连接按钮已解禁：点击调 scm-probe + scm-branches，预填默认分支、展示 head', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse(200, [])) // 列表
      .mockResolvedValueOnce(jsonResponse(200, { head: 'abc123' })) // scm-probe
      .mockResolvedValueOnce(
        jsonResponse(200, {
          branches: [{ name: 'main', head: 'abc123' }],
          default_branch: 'main',
        }),
      ) // scm-branches
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))

    await wrapper.get('button[name="project-new"]').trigger('click')
    const testBtn = wrapper.get('button[name="project-test-connection"]')
    expect((testBtn.element as HTMLButtonElement).disabled).toBe(false)
    await wrapper.get('input[name="project-url"]').setValue('https://x/repo.git')
    await testBtn.trigger('click')

    await vi.waitFor(() => expect(wrapper.text()).toContain('连接成功'))
    const urls = fetchMock.mock.calls.map((c) => (c as [string])[0])
    expect(urls).toContain('/api/v1/projects/scm-probe')
    expect(urls).toContain('/api/v1/projects/scm-branches')
    // 默认分支预填。
    expect(
      (wrapper.get('input[name="project-branch"]').element as HTMLInputElement).value,
    ).toBe('main')
    wrapper.unmount()
  })

  it('保存带 SCM 凭据：POST /projects 含 scm_username/scm_password', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(200, [])) // 列表
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))

    await wrapper.get('button[name="project-new"]').trigger('click')
    fetchMock.mockResolvedValueOnce(
      jsonResponse(201, project(5, 'credproj', 'git', 'https://x/c.git')),
    )
    await wrapper.get('input[name="project-name"]').setValue('credproj')
    await wrapper.get('input[name="project-url"]').setValue('https://x/c.git')
    await wrapper.get('input[name="project-scm-username"]').setValue('alice')
    await wrapper.get('input[name="project-scm-password"]').setValue('hunter2-pw')
    await wrapper.get('button[name="project-save"]').trigger('click')

    await vi.waitFor(() => {
      const post = fetchMock.mock.calls.find(
        (c) =>
          (c as [string, RequestInit])[1]?.method === 'POST' &&
          (c as [string])[0] === '/api/v1/projects',
      )
      expect(post).toBeTruthy()
      const [, init] = post as unknown as [string, RequestInit]
      expect(JSON.parse(init.body as string)).toMatchObject({
        name: 'credproj',
        scm_type: 'git',
        scm_url: 'https://x/c.git',
        scm_username: 'alice',
        scm_password: 'hunter2-pw',
      })
    })
    wrapper.unmount()
  })

  it('新建失败（403 非全局 admin）→ 就地展示错误，表单停留', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []))
    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无项目'))

    await wrapper.get('button[name="project-new"]').trigger('click')
    fetchMock.mockResolvedValue(jsonResponse(403, { code: 'FORBIDDEN', message: '创建项目为全局管理员专属操作' }))
    await wrapper.get('input[name="project-name"]').setValue('proj')
    await wrapper.get('input[name="project-url"]').setValue('https://x/p.git')
    await wrapper.get('button[name="project-save"]').trigger('click')

    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('全局管理员专属'))
    expect(wrapper.text()).toContain('仓库类型') // 表单未收
    wrapper.unmount()
  })
})

describe('ProjectsView Naive UI 迁移（#92）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  const fetchMock = vi.fn()

  /** 包装组件：NMessageProvider + ProjectsView，保证 useMessage 注入可用。 */
  const ProjectsWrapper = defineComponent({
    name: 'ProjectsWrapper',
    setup(_, { attrs }) {
      return () => h(NMessageProvider, () => h(ProjectsView, { ...attrs }))
    },
  })

  function mountView(): VueWrapper {
    return mount(ProjectsWrapper, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('项目列表改用卡片布局（NCard）：每个项目一张卡，显示名称/SCM 类型/默认分支', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, [project(1, 'demo', 'git', 'https://x/a.git'), project(2, 'legacy', 'svn', 'https://svn/x/trunk')]),
    )
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.n-card').length).toBeGreaterThanOrEqual(2))
    expect(wrapper.text()).toContain('demo')
    expect(wrapper.text()).toContain('legacy')
    // 卡片显示 SCM 类型 + 默认分支（git 卡带 main，svn 卡无默认分支标签）。
    const demoCard = wrapper.findAll('.project-card').find((c) => c.text().includes('demo'))!
    expect(demoCard.text()).toContain('git')
    expect(demoCard.text()).toContain('main')
  })

  it('空项目列表显示 NEmpty 空状态 + 创建引导按钮', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []))
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-empty').exists()).toBe(true))
    expect(wrapper.text()).toContain('暂无项目')
    // 空态里的引导按钮：点击展开创建表单。
    await wrapper.find('.project-empty-action').trigger('click')
    expect(wrapper.text()).toContain('仓库类型')
  })

  it('创建项目表单改用 NForm/NFormItem：空值提交触发校验错误，不发网络请求', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []))
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-empty').exists()).toBe(true))

    await wrapper.find('.project-empty-action').trigger('click')
    await wrapper.get('form').trigger('submit')
    await vi.waitFor(() => expect(wrapper.findAll('.n-form-item-feedback').length).toBeGreaterThan(0))
    expect(wrapper.text()).toContain('请输入项目名')
    // 校验失败不发起 POST。
    expect(fetchMock.mock.calls.filter((c) => (c as [string])[0] === '/api/v1/projects')).toHaveLength(1)
  })

  it('SCM URL 字段校验：非 http(s):// 开头 → 格式错误提示，不发网络请求', async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []))
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-empty').exists()).toBe(true))

    await wrapper.find('.project-empty-action').trigger('click')
    await wrapper.get('input[name="project-name"]').setValue('proj')
    await wrapper.get('input[name="project-url"]').setValue('git@github.com:x/y.git')
    await wrapper.get('form').trigger('submit')
    await vi.waitFor(() => expect(wrapper.text()).toContain('仓库 URL 需以 http:// 或 https:// 开头'))
    expect(fetchMock.mock.calls.filter((c) => (c as [string])[0] === '/api/v1/projects')).toHaveLength(1)
  })

  it('测试连接按钮带 NSpin 加载 + NTag 成功/失败徽章（不离开表单）', async () => {
    // 用可手动 resolve 的 deferred 控制探测时长：先断言 NSpin 加载态，
    // 再放行断言 NTag 成功徽章（避免探测瞬间完成导致 NSpin 不可见）。
    let resolveProbe!: (res: Response) => void
    const probeGate = new Promise<Response>((resolve) => {
      resolveProbe = resolve
    })
    const routes = new Map<string, Response>()
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const method = init?.method ?? 'GET'
      if (url === '/api/v1/projects' && method === 'GET') return routes.get('list') ?? jsonResponse(200, [])
      if (url === '/api/v1/projects/scm-probe') return probeGate
      if (url === '/api/v1/projects/scm-branches') {
        return routes.get('branches') ?? jsonResponse(200, { branches: [{ name: 'main', head: 'abc123' }], default_branch: 'main' })
      }
      return jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
    })
    routes.set('list', jsonResponse(200, []))
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-empty').exists()).toBe(true))

    await wrapper.find('.project-empty-action').trigger('click')
    const testBtn = wrapper.find('button[name="project-test-connection"]')
    // NTag 徽章容器（初始隐藏）。
    expect(wrapper.find('.probe-badge').exists()).toBe(false)

    routes.set('branches', jsonResponse(200, { branches: [{ name: 'main', head: 'abc123' }], default_branch: 'main' }))
    await wrapper.get('input[name="project-url"]').setValue('https://x/repo.git')
    await testBtn.trigger('click')

    // 探测中（deferred 未放行）：表单内出现 NSpin 徽章。
    await vi.waitFor(() => expect(wrapper.find('.probe-badge .n-spin').exists()).toBe(true))

    // 放行探测：徽章切为成功 NTag，head 文案展示。
    resolveProbe(jsonResponse(200, { head: 'abc123' }))
    await vi.waitFor(() => expect(wrapper.find('.probe-badge .n-tag').exists()).toBe(true))
    // NTag type=success：绿色主题经 cssVars 落 `--n-color`（成功色 18a058 通道）。
    expect(wrapper.find('.probe-badge .n-tag').attributes('style')).toContain('24, 160, 88')
    expect(wrapper.text()).toContain('连接成功')
  })

  it('创建项目成功 → toast 通知 + 表单收起 + 刷新列表', async () => {
    const routes = new Map<string, Response>()
    fetchMock.mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const method = init?.method ?? 'GET'
      if (url === '/api/v1/projects' && method === 'GET') return routes.get('list') ?? jsonResponse(200, [])
      if (url === '/api/v1/projects' && method === 'POST') return routes.get('create') ?? jsonResponse(201, project(3, 'newproj', 'git', 'https://x/new.git'))
      return jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
    })
    routes.set('list', jsonResponse(200, []))
    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-empty').exists()).toBe(true))

    await wrapper.find('.project-empty-action').trigger('click')
    routes.set('create', jsonResponse(201, project(3, 'newproj', 'git', 'https://x/new.git')))
    await wrapper.get('input[name="project-name"]').setValue('newproj')
    await wrapper.get('input[name="project-url"]').setValue('https://x/new.git')
    await wrapper.get('button[name="project-save"]').trigger('click')

    // toast 通知（NMessageProvider 消息挂载到 body）。
    await vi.waitFor(() => {
      const msg = [...document.querySelectorAll('.n-message')].find((m) => m.textContent?.includes('项目已创建'))
      expect(msg).toBeTruthy()
    })
    expect(wrapper.text()).not.toContain('仓库类型') // 表单收起
    // 成功即刷新列表（再次 GET /projects）。
    await vi.waitFor(() => {
      expect(fetchMock.mock.calls.filter((c) => (c as [string])[0] === '/api/v1/projects')).toHaveLength(3)
    })
  })
})

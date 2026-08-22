// 项目详情行为测试（ADR-0014/0016/0020，票 B4-T3）：pipeline 列表（降级
// 探测）+ 成员角色。只测外部行为，API 层以 fetch mock 驱动。
// - 项目元数据：GET /projects/{name}
// - pipeline 列表：后端无列表端点 → 逐个 GET .../pipelines/{name} 探测
//   （200 存在 / 404 未配置）+ 显式退化标注
// - 成员：GET /projects/{name}/members + GET /users/directory 下拉；
//   保存 = PUT 整组替换（未列入者移除）
// 视图在 onMounted 并发发三类请求（项目/探测/成员）：mock 按 URL 路由而非
// 按调用序，避免并发下 mockResolvedValueOnce 队列被乱序消费。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'

import ProjectDetailView from '@/views/ProjectDetailView.vue'
import { i18n, setLocale } from '@/i18n'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

/** 204 无响应体（204/205 不允许有 body——Response 构造器会拒）。 */
function noContent(): Response {
  return new Response(null, { status: 204 })
}

function project(name: string) {
  return {
    id: 1,
    name,
    scm_type: 'git',
    scm_url: 'https://x/a.git',
    default_branch: 'main',
    created_at: 0,
    updated_at: 0,
  }
}

function pipelineDef() {
  return { definition: { name: 'x', stages: [] }, revision: 1, operator: 'alice', updated_at: 0 }
}

function member(username: string, role: 'viewer' | 'runner' | 'admin') {
  return { user_id: 1, username, role }
}

describe('ProjectDetailView 项目详情（pipeline 探测 + 成员角色）', () => {
  let pinia: Pinia
  let router: Router

  /** URL → 响应的路由表（按最长前缀匹配，防并发乱序 + 子路径优先）。 */
  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    let best: { len: number; res: Response } | null = null
    for (const [prefix, res] of routes) {
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best ? best.res : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
  })

  function setRoute(prefix: string, res: Response): void {
    routes.set(prefix, res)
  }

  /** 包装组件：NMessageProvider + ProjectDetailView，保证 useMessage 注入可用。 */
  const DetailWrapper = defineComponent({
    name: 'DetailWrapper',
    setup(_, { attrs }) {
      return () => h(NMessageProvider, () => h(ProjectDetailView, { ...attrs }))
    },
  })

  function mountView(): VueWrapper {
    return mount(DetailWrapper, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects/demo')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('加载项目元数据（GET /projects/{name}）', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('demo'))
    const [url] = fetchMock.mock.calls[0] as [string]
    expect(url).toBe('/api/v1/projects/demo')
    wrapper.unmount()
  })

  it('pipeline 列表降级探测：200 存在 / 404 未配置 / 其它失败标探测失败 + 退化标注', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    // release 探测遇网络层失败（503，非 404）：不得静默当「未配置」，应标「探测失败」。
    setRoute('/api/v1/projects/demo/pipelines/main', jsonResponse(200, pipelineDef()))
    setRoute('/api/v1/projects/demo/pipelines/release', jsonResponse(503, { code: 'HTTP_ERROR', message: 'boom' }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    // 切到 Pipeline 标签页查看探测结果。
    const tab = wrapper.findAll('.n-tabs-tab').find((x) => x.text().trim() === 'Pipeline')!
    await tab.trigger('click')
    // 探测结果渲染（等待 pipeline 项出现——注意退化标注静态文案里也含「存在」，
    // 须等真实探测结果：pipeline 名渲染即探测已落定）。
    await vi.waitFor(() => expect(wrapper.findAll('.pipeline-item').length).toBe(2))
    expect(wrapper.text()).toContain('pipeline 列表端点尚未交付')
    expect(wrapper.text()).toContain('main')
    expect(wrapper.text()).toContain('存在')
    expect(wrapper.text()).toContain('release')
    expect(wrapper.text()).toContain('探测失败')
    // 503 的 badge 是「探测失败」而非「未配置」（非事实不当事实）——
    // 按 badge 元素断言（静态退化文案里含「未配置」字样，不能按全文）。
    const items = wrapper.findAll('.pipeline-item')
    const releaseBadge = items[1]?.text() ?? ''
    expect(releaseBadge).toContain('探测失败')
    expect(releaseBadge).not.toContain('未配置')
    wrapper.unmount()
  })

  it('pipeline 列表降级探测：404 = 未配置', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    setRoute('/api/v1/projects/demo/pipelines/main', jsonResponse(200, pipelineDef()))
    setRoute('/api/v1/projects/demo/pipelines/release', jsonResponse(404, { code: 'NOT_FOUND', message: 'x' }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const tab = wrapper.findAll('.n-tabs-tab').find((x) => x.text().trim() === 'Pipeline')!
    await tab.trigger('click')
    await vi.waitFor(() => expect(wrapper.findAll('.pipeline-item').length).toBe(2))
    expect(wrapper.text()).toContain('release')
    expect(wrapper.text()).toContain('未配置')
    wrapper.unmount()
  })

  it('成员管理：GET members + GET users/directory 填充下拉；保存 = PUT 整组替换', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    setRoute('/api/v1/projects/demo/members', jsonResponse(200, [member('bob', 'viewer')]))
    setRoute('/api/v1/users/directory', jsonResponse(200, [{ id: 1, username: 'bob' }, { id: 2, username: 'carol' }]))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    // 切到成员标签页。
    const tab = wrapper.findAll('.n-tabs-tab').find((x) => x.text().trim() === '成员')!
    await tab.trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('bob'))

    // 从目录下拉选新成员 carol + 角色 runner → 保存（PUT 整组替换）。
    // NSelect 下拉：点开菜单再点选项（jsdom 下需 virtual-scroll 关闭）。
    const usernameSelect = wrapper.findAll('.member-add-field .n-base-selection')[0]!
    await usernameSelect.trigger('click')
    await vi.waitFor(() => {
      const carol = [...document.querySelectorAll('.n-base-select-option')].find((o) => o.textContent?.trim() === 'carol')
      expect(carol).toBeTruthy()
      ;(carol as HTMLElement).click()
    })
    await new Promise((r) => setTimeout(r, 50))

    // 角色下拉切到 runner（第二个 member-add-field 的 NSelect）。
    const roleSelect = wrapper.findAll('.member-add-field .n-base-selection')[1]!
    await roleSelect.trigger('click')
    await vi.waitFor(() => {
      const runner = [...document.querySelectorAll('.n-base-select-option')].find((o) => o.textContent?.trim() === 'runner')
      expect(runner).toBeTruthy()
      ;(runner as HTMLElement).click()
    })
    await new Promise((r) => setTimeout(r, 50))

    // 保存回读：PUT 后返回新清单。
    setRoute('/api/v1/projects/demo/members', jsonResponse(200, [member('bob', 'viewer'), member('carol', 'runner')]))
    await wrapper.find('button[name="member-save"]').trigger('click')

    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find((c) => (c as unknown as [string, RequestInit])[1]?.method === 'PUT')
      expect(put).toBeTruthy()
      const [url, init] = put as unknown as [string, RequestInit]
      expect(url).toBe('/api/v1/projects/demo/members')
      // PUT 整组替换：现存成员 + 新增成员都包含。
      expect(JSON.parse(init.body as string)).toEqual([
        { username: 'bob', role: 'viewer' },
        { username: 'carol', role: 'runner' },
      ])
    })
    await vi.waitFor(() => expect(wrapper.text()).toContain('成员已保存'))
    wrapper.unmount()
  })

  it('成员管理权限不足（403）→ 就地展示错误，不渲染成员表', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    setRoute('/api/v1/projects/demo/members', jsonResponse(403, { code: 'FORBIDDEN', message: '项目权限不足' }))
    setRoute('/api/v1/users/directory', jsonResponse(403, { code: 'FORBIDDEN', message: '项目权限不足' }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const tab = wrapper.findAll('.n-tabs-tab').find((x) => x.text().trim() === '成员')!
    await tab.trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('项目权限不足'))
    expect(wrapper.find('.member-table').exists()).toBe(false)
    wrapper.unmount()
  })
})

describe('ProjectDetailView Naive UI 迁移（#92）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  /** URL → 响应的路由表（按最长前缀匹配，防并发乱序 + 子路径优先）。 */
  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    let best: { len: number; res: Response } | null = null
    for (const [prefix, res] of routes) {
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best ? best.res : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${url}` })
  })

  function setRoute(prefix: string, res: Response): void {
    routes.set(prefix, res)
  }

  /** 包装组件：NMessageProvider + ProjectDetailView，保证 useMessage 注入可用。 */
  const DetailWrapper = defineComponent({
    name: 'DetailWrapper',
    setup(_, { attrs }) {
      return () => h(NMessageProvider, () => h(ProjectDetailView, { ...attrs }))
    },
  })

  function mountView(): VueWrapper {
    return mount(DetailWrapper, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects/demo')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('内容改用 NTabs 组织：概览 / Pipeline / 成员 / SCM 凭据 四个标签页', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))

    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const tabTitles = wrapper.findAll('.n-tabs-tab').map((t) => t.text().trim())
    expect(tabTitles).toContain('概览')
    expect(tabTitles).toContain('Pipeline')
    expect(tabTitles).toContain('成员')
    expect(tabTitles).toContain('SCM 凭据')
    // 默认落在概览标签页：展示项目元数据。
    expect(wrapper.text()).toContain('demo')
    wrapper.unmount()
  })

  it('SCM 凭据标签页：保存调 PUT /scm-credential（整组替换），成功 toast + 清空表单', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    // 项目 admin 档（成员清单可读 = 是）→ SCM 凭据表单可操作。
    setRoute('/api/v1/projects/demo/members', jsonResponse(200, [member('bob', 'viewer')]))
    setRoute('/api/v1/users/directory', jsonResponse(200, [{ id: 1, username: 'bob' }]))

    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    // 切到 SCM 凭据标签。
    const scmTab = wrapper.findAll('.n-tabs-tab').find((t) => t.text().trim() === 'SCM 凭据')!
    await scmTab.trigger('click')
    await vi.waitFor(() => expect(wrapper.find('input[name="cred-username"]').exists()).toBe(true))

    setRoute('/api/v1/projects/demo/scm-credential', noContent())
    await wrapper.get('input[name="cred-username"]').setValue('alice')
    await wrapper.get('input[name="cred-password"]').setValue('secret')
    await wrapper.get('button[name="cred-save"]').trigger('click')

    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find((c) => (c as unknown as [string, RequestInit])[1]?.method === 'PUT')
      expect(put).toBeTruthy()
      const [url, init] = put as unknown as [string, RequestInit]
      expect(url).toBe('/api/v1/projects/demo/scm-credential')
      expect(JSON.parse(init.body as string)).toEqual({ username: 'alice', password: 'secret' })
    })
    // toast 通知。
    await vi.waitFor(() => {
      const msg = [...document.querySelectorAll('.n-message')].find((m) => m.textContent?.includes('凭据已保存'))
      expect(msg).toBeTruthy()
    })
    wrapper.unmount()
  })

  it('SCM 凭据标签页：用户名密码皆空保存 = 清除凭据，toast 提示已清除', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    // 项目 admin 档（成员清单可读 = 是）→ SCM 凭据表单可操作。
    setRoute('/api/v1/projects/demo/members', jsonResponse(200, [member('bob', 'viewer')]))
    setRoute('/api/v1/users/directory', jsonResponse(200, [{ id: 1, username: 'bob' }]))

    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const scmTab = wrapper.findAll('.n-tabs-tab').find((t) => t.text().trim() === 'SCM 凭据')!
    await scmTab.trigger('click')
    await vi.waitFor(() => expect(wrapper.find('input[name="cred-username"]').exists()).toBe(true))

    setRoute('/api/v1/projects/demo/scm-credential', noContent())
    await wrapper.get('button[name="cred-save"]').trigger('click')

    await vi.waitFor(() => {
      const put = fetchMock.mock.calls.find((c) => (c as unknown as [string, RequestInit])[1]?.method === 'PUT')
      expect(put).toBeTruthy()
      const [, init] = put as unknown as [string, RequestInit]
      // 皆空 → 双 null（后端语义 = 清除凭据）。
      expect(JSON.parse(init.body as string)).toEqual({ username: null, password: null })
    })
    await vi.waitFor(() => {
      const msg = [...document.querySelectorAll('.n-message')].find((m) => m.textContent?.includes('凭据已清除'))
      expect(msg).toBeTruthy()
    })
    wrapper.unmount()
  })

  it('SCM 凭据标签页：测试连接调 POST /test-connection，成功展示 head 徽章', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    // 项目 admin 档（成员清单可读 = 是）→ SCM 凭据表单可操作。
    setRoute('/api/v1/projects/demo/members', jsonResponse(200, [member('bob', 'viewer')]))
    setRoute('/api/v1/users/directory', jsonResponse(200, [{ id: 1, username: 'bob' }]))

    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const scmTab = wrapper.findAll('.n-tabs-tab').find((t) => t.text().trim() === 'SCM 凭据')!
    await scmTab.trigger('click')
    await vi.waitFor(() => expect(wrapper.find('button[name="cred-test-connection"]').exists()).toBe(true))

    setRoute('/api/v1/projects/demo/test-connection', jsonResponse(200, { head: 'deadbeef' }))
    await wrapper.get('button[name="cred-test-connection"]').trigger('click')

    // 成功徽章（NTag），head 文案展示。
    await vi.waitFor(() => expect(wrapper.find('.cred-badge .n-tag').exists()).toBe(true))
    expect(wrapper.text()).toContain('连接成功')
    wrapper.unmount()
  })

  it('SCM 凭据标签页：非项目 admin（成员 403）→ 就地提示需 admin 档，不渲染表单', async () => {
    setRoute('/api/v1/projects/demo', jsonResponse(200, project('demo')))
    setRoute('/api/v1/projects/demo/members', jsonResponse(403, { code: 'FORBIDDEN', message: '项目权限不足' }))
    setRoute('/api/v1/users/directory', jsonResponse(403, { code: 'FORBIDDEN', message: '项目权限不足' }))

    wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.find('.n-tabs').exists()).toBe(true))
    const scmTab = wrapper.findAll('.n-tabs-tab').find((t) => t.text().trim() === 'SCM 凭据')!
    await scmTab.trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('凭据管理需项目 admin 档'))
    expect(wrapper.find('button[name="cred-save"]').exists()).toBe(false)
    wrapper.unmount()
  })
})

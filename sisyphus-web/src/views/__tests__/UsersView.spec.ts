// 用户 / PAT 页行为测试（票 #111 定稿铺开，spec #100；ADR-0014）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，淘汰旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers（fixture 即测试
// 数据）；确定性场景（403/空态/错误态）用 server.use 覆盖。只测外部行为
// （用户可见状态、DOM 事件、网络请求形态断言）。
//
// 覆盖面：双清单首载（用户行 + 角色/状态胶囊徽章 + PAT 行 + 计数副标）、
// 建号（POST /users + is_admin 建号时设 + 409 重名/422 短密码弹窗内报错）、
// 禁用/启用（PATCH + 禁用级联删 PAT 语义见契约 spec）、重置密码
// （PUT /users/{name}/password 204）、PAT 创建（一次性明文仅此一次 + 丢弃
// 即不可找回）、PAT 吊销（NPopconfirm + DELETE）、403 退化态、清单失败重试。
//
// 共享 db 注意：USERS/PATS 是模块级可变态——本文件内测试按序共享状态。
// 命令型用例专用名（carol / e2e-token），读断言锚定不被触碰的 fixture 条目
// （admin/alice/bob、ci-deploy/nightly-cleanup），保证用例间无序依赖。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import UsersView from '@/views/UsersView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

/** 包装组件：NMessageProvider + UsersView，保证 useMessage 注入可用。 */
const Host = defineComponent({
  name: 'UsersHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(UsersView, { ...attrs }))
  },
})

describe('UsersView 用户生命周期 + PAT 一次明文（#111 定稿）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** 经 MSW 观测到的请求（method + path + body 摘要，网络请求形态断言面）。 */
  interface Observed {
    method: string
    path: string
    body: unknown
  }
  let requests: Observed[]

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/users', name: 'admin-users', component: { template: '<div />' } }],
    })
    await router.push('/admin/users')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      void request
        .clone()
        .json()
        .then(
          (body) => requests.push({ method: request.method, path: new URL(request.url).pathname, body }),
          () => requests.push({ method: request.method, path: new URL(request.url).pathname, body: null }),
        )
    })
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    document.body.innerHTML = ''
    vi.restoreAllMocks()
    server.resetHandlers()
    server.events.removeAllListeners()
  })

  afterAll(() => {
    server.close()
  })

  function mountView(): VueWrapper {
    wrapper = mount(Host, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
  }

  function requestsOf(method: string, path: string): Observed[] {
    return requests.filter((r) => r.method === method && r.path === path)
  }

  /** 在已打开的 NModal 内按 name 定位输入并注入输入事件（teleport 到 body）。 */
  async function setModalInput(selector: string, value: string): Promise<void> {
    await vi.waitFor(() => expect(document.querySelector(selector)).toBeTruthy())
    const el = document.querySelector(selector) as HTMLInputElement
    el.value = value
    await el.dispatchEvent(new Event('input'))
  }

  /** 在已打开的 NPopconfirm 弹层内点击「确认」（teleport 到 body）。 */
  async function confirmPopconfirm(): Promise<void> {
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm')).toBeTruthy())
    const btn = [...document.querySelectorAll('.n-popconfirm button')].find(
      (b) => b.textContent?.trim() === '确认',
    )
    expect(btn, 'popconfirm 确认按钮').toBeTruthy()
    await (btn as HTMLElement).click()
  }

  /** 等待：fixture 用户行渲染完成（双清单加载完毕的 a11y 锚）。 */
  async function waitLoaded(w: VueWrapper): Promise<void> {
    await vi.waitFor(() => expect(w.find('[data-testid="user-row-admin"]').exists()).toBe(true))
  }

  it('首载双清单：用户行（角色/状态胶囊徽章 + 计数副标）+ PAT 行（无值形态）', async () => {
    const w = mountView()
    await waitLoaded(w)
    await vi.waitFor(() => expect(w.find('[data-testid="token-row-1"]').exists()).toBe(true))

    // fixture：admin/alice/bob 三用户（按用户名排序）。
    expect(w.findAll('.users-row:not(.tokens-row)')).toHaveLength(3)
    expect(w.text()).toContain('共 3 个用户')

    // 角色/状态徽章取真值：admin=全局管理员(info)/活跃；bob 普通用户/活跃。
    const adminBadges = w.find('[data-testid="user-row-admin"]').findAll('.badge')
    expect(adminBadges.map((b) => b.text())).toEqual(['全局管理员', '活跃'])
    expect(adminBadges[0]!.classes()).toContain('info')
    expect(w.find('[data-testid="user-row-alice"] .badge').text()).toBe('普通用户')

    // PAT（admin 名下 fixture 两条）：名/时间行，永不含令牌值。
    expect(w.text()).toContain('ci-deploy')
    expect(w.text()).toContain('nightly-cleanup')
    expect(w.text()).toContain('共 2 个令牌')
    expect(w.text()).not.toMatch(/sis_[a-z2-7]{43}/)

    // 网络形态：mount 即两个 GET。
    expect(requestsOf('GET', '/api/v1/users')).toHaveLength(1)
    expect(requestsOf('GET', '/api/v1/auth/tokens')).toHaveLength(1)
  })

  it('建号：POST /users { username, password, is_admin }（建号时设全局 admin）+ 刷新入列', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('button[name="user-new"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal input[name="user-username"]')).toBeTruthy())
    await setModalInput('.n-modal input[name="user-username"]', 'carol')
    await setModalInput('.n-modal input[name="user-password"]', 'carol12345')
    // is_admin NSwitch 打开（默认关 → 点击翻为开）。
    await (document.querySelector('.n-modal .user-admin-switch') as HTMLElement).click()
    await (document.querySelector('.n-modal button[name="user-create"]') as HTMLElement).click()

    // 提交形态：is_admin 在建号时显式设。
    await vi.waitFor(() => {
      const post = requestsOf('POST', '/api/v1/users').at(-1)
      expect(post).toBeTruthy()
      expect(post!.body).toEqual({ username: 'carol', password: 'carol12345', is_admin: true })
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('用户已创建'))
    await vi.waitFor(() => expect(w.find('[data-testid="user-row-carol"]').exists()).toBe(true))
    expect(w.find('[data-testid="user-row-carol"] .badge').text()).toBe('全局管理员')
  })

  it('建号 409（重名）→ 弹窗内展示错误，表单停留', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('button[name="user-new"]').trigger('click')
    await setModalInput('.n-modal input[name="user-username"]', 'alice')
    await setModalInput('.n-modal input[name="user-password"]', 'alice12345')
    await (document.querySelector('.n-modal button[name="user-create"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('用户名已存在'),
    )
    // 表单未收（弹窗停留，全局 admin 开关仍在）。
    expect(document.querySelector('.n-modal .user-admin-switch')).toBeTruthy()
  })

  it('建号 422（密码短于 8 位）→ 校验清单在弹窗内就地展示', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('button[name="user-new"]').trigger('click')
    await setModalInput('.n-modal input[name="user-username"]', 'dave')
    await setModalInput('.n-modal input[name="user-password"]', 'short')
    await (document.querySelector('.n-modal button[name="user-create"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal .n-alert')?.textContent).toContain('密码最小长度 8 位'),
    )
    expect(requestsOf('POST', '/api/v1/users').length).toBeGreaterThanOrEqual(1)
    expect(w.find('[data-testid="user-row-dave"]').exists()).toBe(false)
  })

  it('禁用/启用：NSwitch 切换 → PATCH /users/{name} { disabled } + 刷新后徽章翻转', async () => {
    const w = mountView()
    await waitLoaded(w)

    // 初始开关 = 启用态（active）。
    const sw = w.get('[data-testid="user-row-alice"] .user-toggle')
    expect(sw.classes()).toContain('n-switch--active')
    expect(w.find('[data-testid="user-row-alice"] .badge.failed').exists()).toBe(false)

    await sw.trigger('click')

    // PATCH 形态：{ disabled: true }。
    await vi.waitFor(() => {
      const patch = requestsOf('PATCH', '/api/v1/users/alice').at(-1)
      expect(patch).toBeTruthy()
      expect(patch!.body).toEqual({ disabled: true })
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('用户已禁用'))
    await vi.waitFor(() => {
      expect(w.get('[data-testid="user-row-alice"] .user-toggle').classes()).not.toContain('n-switch--active')
    })
    await vi.waitFor(() =>
      expect(w.find('[data-testid="user-row-alice"] .badge.failed').text()).toBe('已禁用'),
    )

    // 收尾：API 直启（恢复共享 fixture，本用例不留痕）。
    await fetch('/api/v1/users/alice', {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ disabled: false }),
    })
  })

  it('重置密码：弹窗 → PUT /users/{name}/password { new_password }（204）+ 弹窗收起', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('[data-testid="user-row-bob"] button[name="user-reset"]').trigger('click')
    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('bob'),
    )
    await setModalInput('.n-modal input[name="reset-password"]', 'newpw12345')
    await (document.querySelector('.n-modal button[name="reset-submit"]') as HTMLElement).click()

    await vi.waitFor(() => {
      const put = requestsOf('PUT', '/api/v1/users/bob/password').at(-1)
      expect(put).toBeTruthy()
      expect(put!.body).toEqual({ new_password: 'newpw12345' })
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('密码已重置'))
    // 弹窗收起：内容响应式清空（关闭动画不完成的 jsdom 以内容消失断言语义）。
    await vi.waitFor(() => {
      expect(document.querySelector('.n-modal')?.textContent ?? '').not.toContain('bob')
    })
  })

  it('PAT 创建：POST /auth/tokens → 一次性明文令牌（仅此一次）+ 丢弃不可找回 + 刷新入列', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('button[name="token-new"]').trigger('click')
    await setModalInput('.n-modal input[name="token-name"]', 'e2e-token')
    await (document.querySelector('.n-modal button[name="token-create"]') as HTMLElement).click()

    // 提交形态：留空过期 = 不带 expires_at。
    await vi.waitFor(() => {
      const post = requestsOf('POST', '/api/v1/auth/tokens').at(-1)
      expect(post).toBeTruthy()
      expect(post!.body).toEqual({ name: 'e2e-token' })
    })

    // 一次性明文令牌弹窗（NCode 明文仅此一次）+ 警示。按内容定位该弹窗。
    let credsModal: Element | undefined
    await vi.waitFor(() => {
      credsModal = [...document.querySelectorAll('.n-modal')].find((m) =>
        /sis_[a-z2-7]{43}/.test(m.textContent ?? ''),
      )
      expect(credsModal).toBeTruthy()
    })
    expect(credsModal!.textContent).toContain('此后任何端点都无法找回')

    // 丢弃后令牌不再可见；刷新后列表含新令牌名（且无值形态）。
    await (credsModal!.querySelector('button[name="token-dismiss"]') as HTMLElement).click()
    await vi.waitFor(() =>
      expect((document.body.textContent ?? '').match(/sis_[a-z2-7]{43}/)).toBeNull(),
    )
    await vi.waitFor(() => expect(w.text()).toContain('e2e-token'))
  })

  it('PAT 吊销：NPopconfirm 确认 → DELETE /auth/tokens/{id}（204）+ 刷新后行消失', async () => {
    const w = mountView()
    await waitLoaded(w)
    await vi.waitFor(() => expect(w.find('[data-testid="token-row-1"]').exists()).toBe(true))

    await w.get('[data-testid="token-row-1"] button[name="token-revoke"]').trigger('click')
    await confirmPopconfirm()

    await vi.waitFor(() => {
      expect(requestsOf('DELETE', '/api/v1/auth/tokens/1')).toHaveLength(1)
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('令牌已吊销'))
    await vi.waitFor(() => expect(w.find('[data-testid="token-row-1"]').exists()).toBe(false))
    expect(w.text()).not.toContain('ci-deploy') // 被吊销条目消失
    expect(w.text()).toContain('nightly-cleanup') // 其余令牌仍在
  })

  it('用户清单失败 → 卡内报错 + 重试恢复（覆盖 200 后恢复）', async () => {
    server.use(
      http.get('/api/v1/users', () =>
        HttpResponse.json({ code: 'INTERNAL', message: '服务内部错误' }, { status: 500 }),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.card-alert').exists()).toBe(true))
    expect(w.text()).toContain('服务内部错误')

    server.use(http.get('/api/v1/users', () => HttpResponse.json([])))
    await w.get('button[name="users-retry"]').trigger('click')
    await vi.waitFor(() => expect(w.text()).toContain('暂无用户'))
    expect(w.find('.card-alert').exists()).toBe(false)
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染双卡', async () => {
    server.use(
      http.get('/api/v1/users', () =>
        HttpResponse.json({ code: 'FORBIDDEN', message: '非全局管理员' }, { status: 403 }),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('仅全局管理员可见'))
    expect(w.findAll('.sisy-card')).toHaveLength(0)
    expect(w.find('button[name="user-new"]').exists()).toBe(false)
  })
})

// 机密页行为测试（票 #110 定稿铺开，spec #100；ADR-0015）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，淘汰旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers（fixture 即测试
// 数据）；确定性场景（错误态/空态）用 server.use 覆盖。只测外部行为
// （用户可见状态、DOM 事件、网络请求形态断言）。
//
// 覆盖面：首载骨架屏 → 项目下拉 + 机密名清单（只列名、值任何端点不回显）、
// 写/覆写弹窗（PUT { value } + 刷新 + 值不回显）、删除（NPopconfirm 确认
// → DELETE + 刷新）、切换项目重载、422 校验清单、无项目空态、清单失败
// 重试、写/覆写审计语义（值不进页面任何角落）。
//
// 共享 db 注意：handlers 的 SECRETS fixture 是模块级可变态——本文件内
// 测试按序共享状态。改动型用例全部落在 fresh-project（初始无机密、无
// 流水线，不被其他页面测试消费），只读断言用未被改动过的 SSH_HOST_KEY，
// 保证用例间无序依赖。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider, NSelect } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import SecretsView from '@/views/SecretsView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

/** 改动型用例专用项目（fixture 初始无机密；不进其他断言）。 */
const MUTATION_PROJECT = 'fresh-project'
/** 只读断言锚（web-app fixture 三名中唯一不被改动型用例触碰的名）。 */
const STABLE_NAME = 'SSH_HOST_KEY'

/** 包装组件：NMessageProvider + SecretsView，保证 useMessage 注入可用。 */
const Host = defineComponent({
  name: 'SecretsHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(SecretsView, { ...attrs }))
  },
})

/** 在已打开的 NPopconfirm 弹层内点击「确认」（teleport 到 body）。 */
async function confirmPopconfirm(): Promise<void> {
  await vi.waitFor(() => expect(document.querySelector('.n-popconfirm')).toBeTruthy())
  const btn = [...document.querySelectorAll('.n-popconfirm button')].find(
    (b) => b.textContent?.trim() === '确认',
  )
  expect(btn, 'popconfirm 确认按钮').toBeTruthy()
  await (btn as HTMLElement).click()
}

/** 打开写弹窗并填入名/值（NModal teleport 到 body——弹窗内元素经 document
 *  定位；原生 value + input 事件驱动 v-model）。 */
async function fillWriteForm(name: string, value: string): Promise<void> {
  await vi.waitFor(() => expect(document.querySelector('.n-modal input[name="secret-name"]')).toBeTruthy())
  const nameInput = document.querySelector('.n-modal input[name="secret-name"]') as HTMLInputElement
  nameInput.value = name
  nameInput.dispatchEvent(new Event('input'))
  const valueInput = document.querySelector('.n-modal textarea[name="secret-value"]') as HTMLTextAreaElement
  valueInput.value = value
  valueInput.dispatchEvent(new Event('input'))
  await new Promise((r) => setTimeout(r, 50))
}

describe('SecretsView 机密只列名 + 写覆写/删 + 切换项目（#110 定稿）', () => {
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
      routes: [{ path: '/admin/secrets', name: 'admin-secrets', component: { template: '<div />' } }],
    })
    await router.push('/admin/secrets')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      void request
        .clone()
        .json()
        .then(
          (body) => requests.push({ method: request.method, path: new URL(request.url).pathname, body }),
          () =>
            requests.push({ method: request.method, path: new URL(request.url).pathname, body: null }),
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

  /** 等 a11y：首个项目（web-app）的清单行渲染完成。 */
  async function waitFirstProjectRows(w: VueWrapper): Promise<void> {
    await vi.waitFor(() => expect(w.find(`[data-testid="secret-row-${STABLE_NAME}"]`).exists()).toBe(true))
  }

  it('首载骨架屏 → 项目下拉（首项目默认选中）+ 机密名清单只列名（值不回显）', async () => {
    const w = mountView()

    // 骨架屏先于数据出现并随后被替换（事实态纪律）。
    expect(w.find('[data-testid="secrets-skeleton"]').exists()).toBe(true)
    await waitFirstProjectRows(w)
    expect(w.find('[data-testid="secrets-skeleton"]').exists()).toBe(false)

    // web-app fixture：三个机密名按名排序；无值形态展示。
    expect(w.find('[data-testid="secret-row-DEPLOY_KEY"]').exists()).toBe(true)
    expect(w.find('[data-testid="secret-row-NPM_TOKEN"]').exists()).toBe(true)

    // 项目下拉含全部项目（NSelect options），首项默认选中。
    const select = w.findComponent(NSelect)
    const options = select.props('options') as { label: string; value: string }[]
    expect(options.length).toBeGreaterThan(3)
    expect(options.map((o) => o.value)).toContain('web-app')
    expect(select.props('value')).toBe('web-app')

    // 语义提示在位（值形态不出现 + ${} 不解析——清单页只见名）。
    expect(w.text()).toContain('机密经 env 注入任务进程')
    expect(w.text()).toContain('${}')

    // 网络形态：GET /api/v1/projects → GET 首项目机密（watch 驱动首载）。
    await vi.waitFor(() => expect(requestsOf('GET', '/api/v1/projects/web-app/secrets').length).toBe(1))
  })

  it('写/覆写弹窗：PUT /projects/{name}/secrets/{secret} { value } + 刷新 + 值不回显', async () => {
    const w = mountView()
    await waitFirstProjectRows(w)
    const countBefore = requestsOf('GET', `/api/v1/projects/${MUTATION_PROJECT}/secrets`).length

    // 切到改动专用项目（fresh-project 初始空清单）。
    await w.findComponent(NSelect).vm.$emit('update:value', MUTATION_PROJECT)
    await vi.waitFor(() => expect(w.find('.secrets-empty').exists()).toBe(true))

    // 页头动作开弹窗（NModal teleport 到 body——弹窗内元素经 document 定位）。
    await w.get('button[name="secret-new"]').trigger('click')
    await fillWriteForm('NEW_KEY', 'super-secret')
    await (document.querySelector('.n-modal button[name="secret-save"]') as HTMLElement).click()

    // 提交形态：机密名在路径段、值在体（mock 收到即弃——响应无值形态）。
    await vi.waitFor(() => {
      const put = requestsOf('PUT', `/api/v1/projects/${MUTATION_PROJECT}/secrets/NEW_KEY`).at(-1)
      expect(put).toBeTruthy()
      expect(put!.body).toEqual({ value: 'super-secret' })
    })

    // 成功 toast（NMessage teleport 到 body）+ 清单刷新（新名在、值不在）。
    await vi.waitFor(() => expect(document.body.textContent).toContain('机密已写入'))
    await vi.waitFor(() => expect(w.find(`[data-testid="secret-row-NEW_KEY"]`).exists()).toBe(true))
    expect(w.text()).not.toContain('super-secret')
    expect(requestsOf('GET', `/api/v1/projects/${MUTATION_PROJECT}/secrets`).length).toBeGreaterThan(countBefore)

    // 收尾：API 直删（恢复共享 fixture，本用例不留痕）。
    await fetch(`/api/v1/projects/${MUTATION_PROJECT}/secrets/NEW_KEY`, { method: 'DELETE' })
  })

  it('删除机密：NPopconfirm 确认 → DELETE + 刷新 + toast', async () => {
    // 造一个可删条目（API 直写——改动专用项目，不碰其他用例的状态）。
    await fetch(`/api/v1/projects/${MUTATION_PROJECT}/secrets/VICTIM_KEY`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ value: 'victim-value' }),
    })

    const w = mountView()
    await waitFirstProjectRows(w)
    await w.findComponent(NSelect).vm.$emit('update:value', MUTATION_PROJECT)
    await vi.waitFor(() => expect(w.find('[data-testid="secret-row-VICTIM_KEY"]').exists()).toBe(true))

    await w.get('[data-testid="secret-delete-VICTIM_KEY"]').trigger('click')
    await confirmPopconfirm()

    await vi.waitFor(() => {
      expect(requestsOf('DELETE', `/api/v1/projects/${MUTATION_PROJECT}/secrets/VICTIM_KEY`).length).toBe(1)
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('机密已删除'))
    // 刷新后行消失。
    await vi.waitFor(() => expect(w.find('[data-testid="secret-row-VICTIM_KEY"]').exists()).toBe(false))
  })

  it('切换项目 → 重新加载该项目机密', async () => {
    const w = mountView()
    await waitFirstProjectRows(w)

    await vi.waitFor(() => expect(requestsOf('GET', '/api/v1/projects/web-app/secrets').length).toBe(1))

    // NSelect 切换项目（update:value 事件驱动 v-model + watch 重载）。
    // api-gateway fixture：只有 DOCKERHUB_TOKEN（未被任何用例改动）。
    await w.findComponent(NSelect).vm.$emit('update:value', 'api-gateway')
    await vi.waitFor(() => expect(w.find('[data-testid="secret-row-DOCKERHUB_TOKEN"]').exists()).toBe(true))
    expect(requestsOf('GET', '/api/v1/projects/api-gateway/secrets').length).toBe(1)
  })

  it('422（机密名非法）→ 校验清单在弹窗内就地展示', async () => {
    const w = mountView()
    await waitFirstProjectRows(w)

    await w.get('button[name="secret-new"]').trigger('click')
    await fillWriteForm('bad name!', 'v')
    await (document.querySelector('.n-modal button[name="secret-save"]') as HTMLElement).click()

    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal .n-alert')?.textContent).toContain('字母数字与下划线'),
    )
    // 弹窗仍开着（提交未成功，可修正重提）；非法名被服务端 422 拒绝——
    // PUT 已发出但清单无新条目（无写入语义）。
    expect(document.querySelector('.n-modal')).toBeTruthy()
    expect(requestsOf('PUT', '/api/v1/projects/web-app/secrets/bad%20name!')).toHaveLength(1)
    expect(w.find('[data-testid="secret-row-bad name!"]').exists()).toBe(false)
  })

  it('无项目 → 空态提示（不渲染项目下拉）', async () => {
    server.use(
      http.get('/api/v1/projects', () => HttpResponse.json([])),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('暂无项目'))
    expect(w.findComponent(NSelect).exists()).toBe(false)
  })

  it('机密清单失败 → 卡内报错 + 重试恢复（覆盖 200 后恢复）', async () => {
    server.use(
      http.get(`/api/v1/projects/${MUTATION_PROJECT}/secrets`, () =>
        HttpResponse.json({ code: 'INTERNAL', message: '服务内部错误' }, { status: 500 }),
      ),
    )
    const w = mountView()
    await waitFirstProjectRows(w)
    await w.findComponent(NSelect).vm.$emit('update:value', MUTATION_PROJECT)
    await vi.waitFor(() => expect(w.find('.card-alert').exists()).toBe(true))

    // 覆盖回 200（MSW 语义：override 叠加，恢复须显式回写）后重试恢复。
    server.use(
      http.get(`/api/v1/projects/${MUTATION_PROJECT}/secrets`, () => HttpResponse.json([])),
    )
    await w.get('button[name="secrets-list-retry"]').trigger('click')
    await vi.waitFor(() => expect(w.find('.secrets-empty').exists()).toBe(true))
    expect(w.find('.card-alert').exists()).toBe(false)
  })

  it('项目清单失败 → 整页报错 + 重试恢复（覆盖 200 后恢复）', async () => {
    server.use(
      http.get('/api/v1/projects', () =>
        HttpResponse.json({ code: 'INTERNAL', message: '服务内部错误' }, { status: 500 }),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.n-alert').exists()).toBe(true))
    expect(w.find('[data-testid="secrets-skeleton"]').exists()).toBe(false)

    // 覆盖回 200 后重试恢复（命中基础 fixture 清单 + 首项目机密）。
    server.use(
      http.get('/api/v1/projects', () =>
        HttpResponse.json(
          [{ id: 1, name: 'web-app', scm_type: 'git', scm_url: 'https://x', default_branch: 'main', created_at: 0, updated_at: 0 }],
        ),
      ),
    )
    await w.get('button[name="secrets-retry"]').trigger('click')
    await vi.waitFor(() => expect(w.find(`[data-testid="secret-row-${STABLE_NAME}"]`).exists()).toBe(true))
    expect(w.find('.n-alert').exists()).toBe(false)
  })
})

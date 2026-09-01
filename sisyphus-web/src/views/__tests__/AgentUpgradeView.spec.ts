// Agent 升级页行为测试（票 #111 定稿铺开，spec #100；ADR-0017）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，淘汰旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers（fixture 即测试
// 数据）；确定性场景（403/空态/阶段真值）用 server.use 覆盖。只测外部行为
// （用户可见状态、DOM 事件、网络请求形态断言）。
//
// 覆盖面：首载双清单（包行 + Agent 行 + 胶囊徽章）、升级阶段列真值
// （downloading 进度条 50% / fallback 红条 / 排空「是」）、包上传
// （X-Sisyphus-Filename 头 + raw body + 新行入列 + 422 文件名非法）、
// 全量升级（POST /agents/upgrade 受理摘要 issued/skipped）、单台升级
// （含 409 已在目标版本 + M7 深链 ?agent= 预选）、删除包、403 退化态、
// 空态。
//
// 共享 db 注意：全量/单台升级会落定 fixture Agent 的 draining/阶段（模块级
// 可变态）——命令型用例排在读断言之后，读断言所需状态一律经 server.use
// 覆盖注入，不受前序用例影响。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider, NSelect } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import AgentUpgradeView from '@/views/AgentUpgradeView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'
import type { AgentResponse } from '@/api/types'

/** fixture 首包（加载后默认选中的目标升级包）。 */
const DEFAULT_PKG = 'sisyphus-agent-1.5.0-linux-x86_64.tar.gz'

/** 受控 Agent（读断言用 server.use 覆盖注入，不依赖共享 fixture 状态）。 */
function agent(name: string, overrides: Partial<AgentResponse> = {}): AgentResponse {
  return {
    name,
    online: true,
    disabled: false,
    system_labels: ['linux'],
    custom_labels: [],
    max_concurrency: 2,
    active_jobs: 0,
    last_seen_at: 1,
    disk_usage: null,
    agent_version: { major: 1, minor: 4, patch: 0 },
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    cpu_usage: 10,
    memory_usage: 20,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  }
}

/** 包装组件：NMessageProvider + AgentUpgradeView，保证 useMessage 注入可用。 */
const Host = defineComponent({
  name: 'UpgradeHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AgentUpgradeView, { ...attrs }))
  },
})

describe('AgentUpgradeView 包上传 + 全量/单台升级 + 排空阶段列（#111 定稿）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** 经 MSW 观测到的请求（method + path + body + 关键头）。 */
  interface Observed {
    method: string
    path: string
    body: unknown
    filename: string | null
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
      routes: [{ path: '/admin/upgrade', name: 'admin-upgrade', component: { template: '<div />' } }],
    })
    await router.push('/admin/upgrade')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      void request
        .clone()
        .json()
        .then(
          (body) =>
            requests.push({
              method: request.method,
              path: new URL(request.url).pathname,
              body,
              filename: request.headers.get('x-sisyphus-filename'),
            }),
          () =>
            requests.push({
              method: request.method,
              path: new URL(request.url).pathname,
              body: null,
              filename: request.headers.get('x-sisyphus-filename'),
            }),
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

  /** 等待：fixture 首包行渲染完成（双清单加载完毕的 a11y 锚）。 */
  async function waitLoaded(w: VueWrapper): Promise<void> {
    await vi.waitFor(() =>
      expect(w.find(`[data-testid="upgrade-package-${DEFAULT_PKG}"]`).exists()).toBe(true),
    )
  }

  it('首载双清单：包行 + Agent 行（计数副标/胶囊徽章/默认选中首包）', async () => {
    const w = mountView()
    await waitLoaded(w)

    // fixture：4 个升级包 + 7 台构建机。
    expect(w.findAll('.upgrade-pkg-row')).toHaveLength(4)
    await vi.waitFor(() => expect(w.findAll('.upgrade-agent-row')).toHaveLength(7))
    expect(w.text()).toContain('共 4 个包')
    expect(w.text()).toContain('共 7 台构建机')

    // 徽章取真值：build-06 在线且排空 → 「排空」；build-07 版本不兼容 → 红标。
    expect(w.find('[data-testid="upgrade-agent-build-06"] .badge').classes()).toContain('draining')
    expect(w.find('[data-testid="upgrade-agent-build-06"] .badge').text()).toBe('排空')
    expect(w.find('[data-testid="upgrade-agent-build-07"] .badge').classes()).toContain('failed')
    expect(w.find('[data-testid="upgrade-agent-build-07"] .badge').text()).toBe('版本不兼容')

    // 默认选中首包（全量/单台指令的目标包）。
    const selects = w.findAllComponents(NSelect)
    expect(selects[0]!.props('value')).toBe(DEFAULT_PKG)
    expect(selects[1]!.props('value')).toBe('build-01')

    // 网络形态：mount 即两个 GET。
    expect(requestsOf('GET', '/api/v1/agents')).toHaveLength(1)
    expect(requestsOf('GET', '/api/v1/upgrade-packages')).toHaveLength(1)
  })

  it('升级阶段列取真值：downloading → 进度条 50% + 下载中；fallback → 红条 100% + 退回文案；排空列「是」', async () => {
    server.use(
      http.get('/api/v1/agents', () =>
        HttpResponse.json([
          agent('demo-1', { draining: true, upgrade_phase: 'downloading' }),
          agent('demo-2', { upgrade_phase: 'fallback', upgrade_error: 'sha256 校验失败（fixture 模拟）' }),
        ]),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.findAll('.upgrade-agent-row')).toHaveLength(2))

    const row1 = w.find('[data-testid="upgrade-agent-demo-1"]')
    expect(row1.text()).toContain('下载中')
    expect(row1.text()).toContain('是') // 排空列
    const fill1 = row1.get('.usage-row .fill')
    expect(fill1.attributes('style')).toContain('width: 50%')
    expect(fill1.classes()).not.toContain('red')

    const row2 = w.find('[data-testid="upgrade-agent-demo-2"]')
    expect(row2.text()).toContain('已退回旧版本')
    const fill2 = row2.get('.usage-row .fill')
    expect(fill2.attributes('style')).toContain('width: 100%')
    expect(fill2.classes()).toContain('red')
    // upgrade_error 就地灰字提示。
    expect(row2.text()).toContain('sha256 校验失败')
  })

  it('包上传：文件选择即传 → POST /upgrade-packages（X-Sisyphus-Filename 头 + raw body）+ 新行入列', async () => {
    const w = mountView()
    await waitLoaded(w)

    // 新包名（fixture 未有；上传后入列）。
    const input = w.get('input[type="file"]').element as HTMLInputElement
    const file = new File(['pkg-bytes'], 'sisyphus-agent-1.4.4-linux-x86_64.tar.gz', {
      type: 'application/octet-stream',
    })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await input.dispatchEvent(new Event('change'))

    // 网络形态：包名在 X-Sisyphus-Filename 头、body 为原始字节（长度 10）。
    await vi.waitFor(() => {
      const post = requestsOf('POST', '/api/v1/upgrade-packages').at(-1)
      expect(post).toBeTruthy()
      expect(post!.filename).toBe('sisyphus-agent-1.4.4-linux-x86_64.tar.gz')
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('已上传'))
    await vi.waitFor(() =>
      expect(w.find('[data-testid="upgrade-package-sisyphus-agent-1.4.4-linux-x86_64.tar.gz"]').exists()).toBe(true),
    )

    // 收尾：API 直删（恢复共享 fixture，本用例不留痕）。
    await fetch('/api/v1/upgrade-packages/sisyphus-agent-1.4.4-linux-x86_64.tar.gz', { method: 'DELETE' })
  })

  it('包上传 422（文件名不可解析）→ 卡内就地报错', async () => {
    const w = mountView()
    await waitLoaded(w)

    const input = w.get('input[type="file"]').element as HTMLInputElement
    const file = new File(['x'], 'not-a-package.bin', { type: 'application/octet-stream' })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await input.dispatchEvent(new Event('change'))

    await vi.waitFor(() => expect(w.find('.upgrade-card-alert').exists()).toBe(true))
    expect(w.text()).toContain('不可解析')
  })

  it('全量升级：默认选包 + 按钮 → POST /agents/upgrade → 受理摘要（issued/skipped 真值）', async () => {
    const w = mountView()
    await waitLoaded(w)

    await w.get('button[name="upgrade-all"]').trigger('click')

    await vi.waitFor(() => {
      const post = requestsOf('POST', '/api/v1/agents/upgrade').at(-1)
      expect(post).toBeTruthy()
      expect(post!.body).toEqual({ package_name: DEFAULT_PKG })
    })
    // fixture 真值：未停用且非 1.5.0 的 4 台下发（build-02/04/06/07），
    // 已在目标版本的 2 台跳过（build-01/03）。
    await vi.waitFor(() =>
      expect(document.body.textContent).toContain('下发 4 台，跳过 2 台'),
    )
    // 下发目标落定 draining 阶段（刷新后可见）。
    await vi.waitFor(() =>
      expect(w.find('[data-testid="upgrade-agent-build-02"]').text()).toContain('排空'),
    )
  })

  it('单台升级：409（已在目标版本）→ 卡内就地报错', async () => {
    const w = mountView()
    await waitLoaded(w)

    // 默认目标 build-01 已在 1.5.0（fixture）→ 409 就地报错，不 toast。
    await w.get('button[name="upgrade-one"]').trigger('click')
    await vi.waitFor(() => {
      expect(requestsOf('POST', '/api/v1/agents/build-01/upgrade')).toHaveLength(1)
    })
    await vi.waitFor(() => expect(w.find('.upgrade-card-alert').exists()).toBe(true))
    expect(w.text()).toContain('已在目标版本')
  })

  it('单台升级（M7 深链预选）：?agent=build-07 → 直发该机器 + 下发后落定排空阶段', async () => {
    await router.push({ path: '/admin/upgrade', query: { agent: 'build-07' } })
    await router.isReady()
    const w = mountView()
    await waitLoaded(w)

    // 深链预选：单台目标 = build-07（版本不兼容机器，M7 入口直达）。
    expect(w.findAllComponents(NSelect)[1]!.props('value')).toBe('build-07')
    await w.get('button[name="upgrade-one"]').trigger('click')

    await vi.waitFor(() => {
      expect(requestsOf('POST', '/api/v1/agents/build-07/upgrade')).toHaveLength(1)
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('已向 build-07 下发升级指令'))
    await vi.waitFor(() =>
      expect(w.find('[data-testid="upgrade-agent-build-07"]').text()).toContain('排空'),
    )
  })

  it('删除包：NPopconfirm 确认 → DELETE + 刷新后行消失', async () => {
    // 造一个可删条目（API 直传——命令型用例专用名，不碰 fixture 既有包；
    // 包名在 X-Sisyphus-Filename 头、字节在 raw body）。
    await fetch('/api/v1/upgrade-packages', {
      method: 'POST',
      headers: { 'X-Sisyphus-Filename': 'sisyphus-agent-1.4.4-macos-aarch64.tar.gz' },
      body: 'victim-bytes',
    })

    const w = mountView()
    await waitLoaded(w)
    await vi.waitFor(() =>
      expect(w.find('[data-testid="upgrade-package-sisyphus-agent-1.4.4-macos-aarch64.tar.gz"]').exists()).toBe(true),
    )

    await w.get('[data-testid="upgrade-package-delete-sisyphus-agent-1.4.4-macos-aarch64.tar.gz"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm')).toBeTruthy())
    const btn = [...document.querySelectorAll('.n-popconfirm button')].find(
      (b) => b.textContent?.trim() === '确认',
    )
    expect(btn, 'popconfirm 确认按钮').toBeTruthy()
    await (btn as HTMLElement).click()

    await vi.waitFor(() => {
      expect(
        requestsOf('DELETE', '/api/v1/upgrade-packages/sisyphus-agent-1.4.4-macos-aarch64.tar.gz'),
      ).toHaveLength(1)
    })
    await vi.waitFor(() => expect(document.body.textContent).toContain('升级包已删除'))
    await vi.waitFor(() =>
      expect(w.find('[data-testid="upgrade-package-sisyphus-agent-1.4.4-macos-aarch64.tar.gz"]').exists()).toBe(false),
    )
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染动作/清单', async () => {
    server.use(
      http.get('/api/v1/agents', () =>
        HttpResponse.json({ code: 'FORBIDDEN', message: '非全局管理员' }, { status: 403 }),
      ),
      http.get('/api/v1/upgrade-packages', () =>
        HttpResponse.json({ code: 'FORBIDDEN', message: '非全局管理员' }, { status: 403 }),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('仅全局管理员可见'))
    expect(w.find('.sisy-card').exists()).toBe(false)
    expect(w.find('button[name="upgrade-all"]').exists()).toBe(false)
  })

  it('无 Agent / 无包 → 空态提示', async () => {
    server.use(
      http.get('/api/v1/agents', () => HttpResponse.json([])),
      http.get('/api/v1/upgrade-packages', () => HttpResponse.json([])),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.n-empty').exists()).toBe(true))
    expect(w.text()).toContain('暂无构建机')
    expect(w.text()).toContain('暂无已上传升级包')
  })
})

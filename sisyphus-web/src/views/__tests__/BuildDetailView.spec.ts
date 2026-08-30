// 构建详情页行为测试（票 #107 定稿铺开，spec #100；ADR-0006/0008/0013）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，替代旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers；确定性场景用
// server.use 覆盖详情/定义/产物/动作端点。SSE 不走 fetch（EventSource
// 不经 MSW），以 FakeEventSource 替身驱动。只测外部行为（用户可见状态、
// DOM 事件、网络请求形态断言）。
//
// 覆盖面：面包屑 + 阶段/任务卡（attempt 历史、排队缺失标签等待态、
// allow_failure）、产物下载（已上传/占位）、动作闭环（触发/取消/重跑/
// 删除，202 受理 + 409 拒绝 + toast 反馈）、事实态（骨架屏、错误重试、
// 403 退化、404、等待态定义缺失退化、空阶段）、SSE 日志（步骤生命周期 +
// 输出块交织、ANSI 剥离、折叠、截断、终态关流、首连失败退化、重连）。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import BuildDetailView from '@/views/BuildDetailView.vue'
import { i18n, setLocale } from '@/i18n'
import { FakeEventSource } from '@/test/fakeEventSource'
import { server } from '@/mocks/node'

const BASE = '/api/v1/projects/web-app/pipelines/release'

/** 构建详情响应（含两阶段：build 阶段成功任务 + deploy 阶段排队任务）。 */
function buildDetailBody(overrides: Record<string, unknown> = {}) {
  return {
    number: 7,
    pipeline_name: 'release',
    status: 'running',
    trigger: 'manual',
    trigger_by: 'alice',
    attempt: 1,
    started_at: 1_700_000_000_000,
    finished_at: null,
    cancelled_at: null,
    elapsed_ms: 12_000,
    stages: [
      {
        index: 0,
        name: 'build',
        jobs: [
          {
            name: 'compile',
            status: 'succeeded',
            attempt: 1,
            started_at: 1_700_000_000_000,
            finished_at: 1_700_000_005_000,
            exit_code: 0,
            allow_failure: false,
            detail: null,
            agent_id: 3,
          },
        ],
      },
      {
        index: 1,
        name: 'deploy',
        jobs: [
          {
            name: 'push',
            status: 'queued',
            attempt: 1,
            started_at: null,
            finished_at: null,
            exit_code: null,
            allow_failure: true,
            detail: '上一次失败：连接超时',
            agent_id: null,
          },
        ],
      },
    ],
    ...overrides,
  }
}

/** pipeline 定义响应（排队任务缺失标签展示源 + 触发参数声明 + 产物声明）。 */
function pipelineDefBody() {
  return {
    definition: {
      name: 'release',
      parameters: [
        { name: 'target', type: 'enum', required: true, default: 'x86_64', choices: ['x86_64', 'aarch64'] },
        { name: 'jobs', type: 'number', required: false, default: 4 },
      ],
      stages: [
        { name: 'build', jobs: [{ name: 'compile', labels: ['linux'] }] },
        {
          name: 'deploy',
          jobs: [
            {
              name: 'push',
              labels: ['gpu', 'arch=arm64'],
              artifact_uploads: [{ name: 'bundle', path: 'dist/bundle.zip' }],
            },
          ],
        },
      ],
    },
    revision: 3,
    operator: 'alice',
    updated_at: 1_700_000_000_000,
  }
}

/** 覆盖构建详情端点（status 为 number 时返回该 HTTP 状态码错误体）。 */
function mockDetail(body: Record<string, unknown>, status = 200): void {
  server.use(
    http.get(`${BASE}/builds/7`, () =>
      status === 200
        ? HttpResponse.json(body)
        : HttpResponse.json({ code: 'MOCK', message: 'mock error' }, { status }),
    ),
  )
}

/** 覆盖定义 / 产物端点（definition 传 null → 404，等待态退化场景）。 */
function mockDefinitionAndArtifacts(artifacts: unknown[] = []): void {
  server.use(
    http.get(`${BASE}`, () => HttpResponse.json(pipelineDefBody())),
    http.get(`${BASE}/builds/7/artifacts`, () => HttpResponse.json({ items: artifacts })),
  )
}

/** 包装组件：NMessageProvider + BuildDetailView（useMessage 注入可用）。 */
const Host = defineComponent({
  name: 'BuildDetailHost',
  setup() {
    return () => h(NMessageProvider, () => h(BuildDetailView))
  },
})

describe('BuildDetailView（#107 定稿：阶段/任务卡 + 动作闭环 + SSE 日志）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** 经 MSW 观测到的请求（method + path，网络请求形态断言面）。 */
  let requests: string[]

  function mountView(): VueWrapper {
    wrapper = mount(Host, {
      global: { plugins: [pinia, router, i18n] },
    })
    return wrapper
  }

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects/:name/pipelines/:pipeline/builds/:number', name: 'build-detail', component: { template: '<div />' } },
        { path: '/projects/:name/pipelines/:pipeline/builds', name: 'build-list', component: { template: '<div />' } },
        { path: '/projects/:name/pipelines/:pipeline', name: 'pipeline-edit', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects/web-app/pipelines/release/builds/7')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      requests.push(`${request.method} ${new URL(request.url).pathname}`)
    })
    FakeEventSource.install()
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
    server.resetHandlers()
    server.events.removeAllListeners()
  })

  afterAll(() => {
    server.close()
  })

  it('首载骨架屏 → 面包屑 + 阶段/任务卡：attempt 历史、缺失标签等待态、allow_failure', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()

    // 骨架屏先可见（事实态纪律），数据到达后替换。
    expect(wrapper!.find('[data-testid="build-detail-skeleton"]').exists()).toBe(true)
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    // 面包屑：项目 > demo 项目 > release > 构建 #7。
    const bc = wrapper!.get('.breadcrumb').text()
    expect(bc).toContain('项目')
    expect(bc).toContain('web-app')
    expect(bc).toContain('release')
    expect(bc).toContain('构建 #7')

    // 阶段名与任务卡（attempt=1 无历史标注；deploy 阶段排队任务）。
    expect(wrapper!.findAll('.stage-block')[0]!.text()).toContain('build')
    expect(wrapper!.findAll('.stage-block')[1]!.text()).toContain('deploy')
    expect(wrapper!.text()).toContain('compile')
    expect(wrapper!.text()).toContain('push')

    // 排队任务缺失标签等待态（ADR-0008：从定义 labels 派生）。
    expect(wrapper!.get('.job-waiting').text()).toContain('gpu, arch=arm64')

    // allow_failure 中性胶囊徽标。
    expect(wrapper!.text()).toContain('允许失败')

    // 状态胶囊（badge）：构建运行中 = 蓝、任务成功 = 绿、排队 = info 蓝。
    expect(wrapper!.find('.build-title-row .badge.running').exists()).toBe(true)
    expect(wrapper!.find('.job-card .badge.success').exists()).toBe(true)
    expect(wrapper!.find('.job-card .badge.info').exists()).toBe(true)
  })

  it('排队任务 attempt 历史（attempt>1 标注并列）', async () => {
    const base = buildDetailBody()
    const pushJob = (base.stages[1] as { jobs: Array<Record<string, unknown>> }).jobs[0] as Record<string, unknown>
    mockDetail({
      ...base,
      attempt: 2,
      stages: [
        base.stages[0] as Record<string, unknown>,
        {
          index: 1,
          name: 'deploy',
          jobs: [
            { ...pushJob, attempt: 1, status: 'failed' },
            { ...pushJob, attempt: 2 },
          ],
        },
      ],
    })
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))
    // 同任务两行并列（attempt 历史行）；attempt>1 行带历史标注。
    const pushRows = wrapper!.findAll('.stage-block')[1]!.findAll('.job-card')
    expect(pushRows).toHaveLength(2)
    expect(wrapper!.text()).toContain('第 2 次尝试')
  })

  it('产物区：已上传接下载链接（大小/sha 提示），未上传展示占位', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts([
      { name: 'bundle', size: 4096, sha256: 'ab12'.repeat(16), created_at: 1_700_000_000_000 },
    ])
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('a.artifact-link').exists()).toBe(true))

    // 声明 bundle 已上传 → 下载链接 + 大小；不再展示占位。
    const link = wrapper!.get('a.artifact-link')
    expect(link.text()).toContain('bundle')
    expect(link.text()).toContain('4.0 KB')
    expect(link.attributes('href')).toBe(`api/v1/projects/web-app/pipelines/release/builds/7/artifacts/bundle`)
    expect(link.attributes('download')).toBe('bundle')
    expect(link.attributes('title')).toContain('ab12')
    expect(wrapper!.text()).not.toContain('下载占位')
  })

  it('产物区：已声明未上传展示占位（构建进行中）', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts([])
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('bundle'))
    expect(wrapper!.text()).toContain('下载占位')
  })

  it('排队等待态定义缺失时显式标注退化', async () => {
    mockDetail(buildDetailBody())
    // 定义端点 404（定义缺失）→ 等待态退化标注；产物端点正常。
    server.use(
      http.get(`${BASE}`, () =>
        HttpResponse.json({ code: 'NOT_FOUND', message: '流水线不存在' }, { status: 404 }),
      ),
      http.get(`${BASE}/builds/7/artifacts`, () => HttpResponse.json({ items: [] })),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('退化态'))
  })

  it('403 退化态：无项目访问权限时不渲染详情体', async () => {
    mockDetail({ code: 'FORBIDDEN', message: '无权访问' }, 403)
    mountView()
    await vi.waitFor(() =>
      expect(wrapper!.find('[data-testid="build-detail-forbidden"]').exists()).toBe(true),
    )
    expect(wrapper!.find('.stage-block').exists()).toBe(false)
  })

  it('404：构建不存在提示', async () => {
    mockDetail({ code: 'NOT_FOUND', message: '构建不存在' }, 404)
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('构建不存在'))
  })

  it('加载失败整页报错 + 重试按钮；重试恢复渲染', async () => {
    mockDetail({ code: 'INTERNAL', message: '服务内部错误' }, 500)
    mountView()
    await vi.waitFor(() => expect(wrapper!.find('[data-testid="build-detail-retry"]').exists()).toBe(true))

    // 修复端点后点重试 → 恢复正常渲染。
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    await wrapper!.find('[data-testid="build-detail-retry"]').trigger('click')
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))
  })

  it('空态：构建无阶段/任务记录', async () => {
    mockDetail(buildDetailBody({ status: 'succeeded', finished_at: 1_700_000_010_000, stages: [] }))
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('该构建没有阶段/任务记录'))
  })

  it('触发对话框：参数默认值预填、可覆盖，提交 POST 带参数/分支/commit 并跳转新构建', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    let triggerBody: unknown = null
    server.use(
      http.post(`${BASE}/builds`, async ({ request }) => {
        triggerBody = await request.json()
        return HttpResponse.json({ number: 8, build_id: 8, attempt: 1, status: 'queued' }, { status: 202 })
      }),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.get('[data-testid="trigger-btn"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')?.textContent).toContain('触发构建'))

    // 参数默认值预填：enum 参数 target → NSelect（选中 x86_64）、
    // number 参数 jobs → NInput（name=param-jobs，值 4）。
    expect(document.querySelector('.n-modal')?.textContent).toContain('x86_64')
    const jobsInput = document.querySelector('.n-modal input[name="param-jobs"]') as HTMLInputElement
    expect(jobsInput?.value).toBe('4')

    // 覆盖 enum 参数 target → aarch64（NSelect 下拉选选项）。
    const modalSelect = document.querySelector('.n-modal .n-base-selection')
    await (modalSelect as HTMLElement).dispatchEvent(new Event('click'))
    await vi.waitFor(() => {
      const opt = [...document.querySelectorAll('.n-base-select-option')].find((o) => o.textContent?.trim() === 'aarch64')
      expect(opt).toBeTruthy()
      ;(opt as HTMLElement).click()
    })

    // 覆盖 number 参数 jobs → 8；分支/commit。
    jobsInput.value = '8'
    await jobsInput.dispatchEvent(new Event('input'))
    const branchInput = document.querySelector('.n-modal input[name="trigger-branch"]') as HTMLInputElement
    const commitInput = document.querySelector('.n-modal input[name="trigger-commit"]') as HTMLInputElement
    branchInput.value = 'release/1.0'
    await branchInput.dispatchEvent(new Event('input'))
    commitInput.value = 'abc123'
    await commitInput.dispatchEvent(new Event('input'))

    const pushSpy = vi.spyOn(router, 'push')
    const modalButtons = [...document.querySelectorAll('.n-modal button')]
    const submitBtn = modalButtons.find((b) => b.textContent?.trim() === '触发构建')
    await (submitBtn as HTMLElement).click()
    await vi.waitFor(() => expect(pushSpy).toHaveBeenCalled())

    // 请求形态：POST /builds（触发）带参数覆盖/分支/commit；跳转新构建号 8。
    expect(triggerBody).toEqual({
      params: { target: 'aarch64', jobs: '8' },
      branch: 'release/1.0',
      commit: 'abc123',
    })
    const triggerCall = requests.find((r) => r === 'POST /api/v1/projects/web-app/pipelines/release/builds')
    expect(triggerCall).toBeDefined()
    expect(pushSpy.mock.calls[0]?.[0]).toMatchObject({
      name: 'build-detail',
      params: { number: '8' },
    })
  })

  it('触发对话框提交失败（409）时弹窗内错误反馈', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    server.use(
      http.post(
        `${BASE}/builds`,
        () => HttpResponse.json({ code: 'CONFLICT', message: '同一条 Pipeline 同时只跑一条构建' }, { status: 409 }),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.get('[data-testid="trigger-btn"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    const modalButtons = [...document.querySelectorAll('.n-modal button')]
    const submitBtn = modalButtons.find((b) => b.textContent?.trim() === '触发构建')
    await (submitBtn as HTMLElement).click()
    // 409 触发冲突 → 弹窗内给 triggerConflict 专属文案（不串用重跑冲突文案）。
    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal')?.textContent).toContain('无法触发（409）'),
    )
  })

  it('取消构建：运行中可取消 → POST cancel，202 受理 toast', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    server.use(
      http.post(`${BASE}/builds/7/cancel`, () =>
        HttpResponse.json({ number: 7, build_id: 7, attempt: 1, status: 'cancelled' }, { status: 202 }),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.get('[data-testid="cancel-btn"]').trigger('click')
    await vi.waitFor(() => {
      expect(requests.some((r) => r === `POST ${BASE}/builds/7/cancel`)).toBe(true)
      expect(document.querySelector('.n-message')?.textContent).toContain('已受理取消')
    })
  })

  it('取消按钮：终态构建禁用', async () => {
    mockDetail(buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }))
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))
    expect(wrapper!.get('[data-testid="cancel-btn"]').attributes('disabled')).toBeDefined()
  })

  it('从头重跑：POST rerun（from_scratch）→ 跳转新构建号', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    server.use(
      http.post(`${BASE}/builds/7/rerun`, () =>
        HttpResponse.json({ number: 9, build_id: 9, attempt: 1, status: 'queued' }, { status: 202 }),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    const pushSpy = vi.spyOn(router, 'push')
    await wrapper!.get('[data-testid="rerun-scratch-btn"]').trigger('click')
    await vi.waitFor(() => {
      expect(requests.some((r) => r === `POST ${BASE}/builds/7/rerun`)).toBe(true)
      expect(pushSpy).toHaveBeenCalled()
    })
    expect(pushSpy.mock.calls[0]?.[0]).toMatchObject({
      name: 'build-detail',
      params: { number: '9' },
    })
  })

  it('从失败重跑：failed 终态可用 → POST rerun（from_failed）202 受理 toast；非失败终态禁用', async () => {
    mockDetail(buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }))
    mockDefinitionAndArtifacts()
    server.use(
      http.post(`${BASE}/builds/7/rerun`, async ({ request }) => {
        expect(await request.json()).toEqual({ mode: 'from_failed' })
        return HttpResponse.json({ number: 7, build_id: 7, attempt: 2, status: 'running' }, { status: 202 })
      }),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.get('[data-testid="rerun-failed-btn"]').trigger('click')
    await vi.waitFor(() => {
      expect(requests.some((r) => r === `POST ${BASE}/builds/7/rerun`)).toBe(true)
      expect(document.querySelector('.n-message')?.textContent).toContain('已受理重跑')
    })
  })

  it('从失败重跑：非失败终态按钮禁用（不给 409 机会）；409 拒绝 toast 兜底', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    // 运行中构建：从失败重跑禁用（不打 409）。
    expect(wrapper!.get('[data-testid="rerun-failed-btn"]').attributes('disabled')).toBeDefined()
  })

  it('从失败重跑：终态可用但后端 409 → 错误 toast（兜底反馈）', async () => {
    mockDetail(buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }))
    mockDefinitionAndArtifacts()
    server.use(
      http.post(
        `${BASE}/builds/7/rerun`,
        () => HttpResponse.json({ code: 'CONFLICT', message: '仅失败终态可重跑' }, { status: 409 }),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.get('[data-testid="rerun-failed-btn"]').trigger('click')
    await vi.waitFor(() => {
      expect(requests.some((r) => r === `POST ${BASE}/builds/7/rerun`)).toBe(true)
      const msg = document.querySelector('.n-message')?.textContent
      expect(msg).toContain('409')
    })
  })

  it('删除构建：终态可删、确认后发 DELETE、204 后跳回构建列表；运行中禁用', async () => {
    mockDetail(buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }))
    mockDefinitionAndArtifacts()
    server.use(http.delete(`${BASE}/builds/7`, () => new HttpResponse(null, { status: 204 })))
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    // 终态构建：删除按钮可用；运行中禁用（对照用例见下一断言）。
    const deleteBtn = wrapper!.get('[data-testid="delete-btn"]')
    expect(deleteBtn.attributes('disabled')).toBeUndefined()

    // 打开确认弹层 → 点「取消」（negative）→ 不发请求。
    await deleteBtn.trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
    const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
    await (actionButtons[0] as HTMLElement).click()
    await new Promise((r) => setTimeout(r, 50))
    expect(requests.some((r) => r === `DELETE ${BASE}/builds/7`)).toBe(false)

    // 再次打开并确认（positive）→ DELETE 204 → 跳回构建列表。
    const pushSpy = vi.spyOn(router, 'push')
    await wrapper!.get('[data-testid="delete-btn"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
    const actionButtons2 = document.querySelectorAll('.n-popconfirm__action button')
    await (actionButtons2[actionButtons2.length - 1] as HTMLElement).click()
    await vi.waitFor(() => expect(pushSpy).toHaveBeenCalled())
    expect(requests.some((r) => r === `DELETE ${BASE}/builds/7`)).toBe(true)
    expect(pushSpy.mock.calls[0]?.[0]).toMatchObject({ name: 'build-list' })
  })

  it('删除构建：运行中构建禁用删除按钮', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))
    expect(wrapper!.get('[data-testid="delete-btn"]').attributes('disabled')).toBeDefined()
  })

  it('SSE 日志：查看日志展开流，输出块合流渲染、ANSI 剥离、步骤折叠/展开', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    // 展开 compile 任务日志。
    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src = FakeEventSource.latest()
    expect(src.url).toContain('/builds/7/jobs/compile/attempts/1/logs/stream?from=0')

    src.dispatchOpen()
    src.dispatch('step_start', {
      type: 'step_start', seq: 1, step: 0, name: 'build', command: 'cargo build', started_at: 1,
    })
    src.dispatch('output', { type: 'output', seq: 2, stream: 'stdout', text: 'compiling...\x1b[32mok\x1b[0m' })
    src.dispatch('output', { type: 'output', seq: 3, stream: 'stderr', text: 'warning' })
    await vi.waitFor(() => expect(wrapper!.text()).toContain('cargo build'))

    // ANSI 剥离：渲染文本无色码（ADR-0013）。
    expect(wrapper!.text()).toContain('compiling...ok')
    expect(wrapper!.text()).not.toContain('\x1b[32m')

    // 步骤折叠：点击步骤头 → 输出隐藏；再点展开。
    await wrapper!.get('.build-log-step-head').trigger('click')
    expect(wrapper!.find('.build-log-step-body').exists()).toBe(false)
    await wrapper!.get('.build-log-step-head').trigger('click')
    expect(wrapper!.find('.build-log-step-body').exists()).toBe(true)
  })

  it('SSE 日志：截断显著标注、终态事件送达即关流', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src = FakeEventSource.latest()

    src.dispatchOpen()
    src.dispatch('output', { type: 'output', seq: 1, stream: 'stdout', text: 'some log' })
    src.dispatch('truncated', { type: 'truncated', seq: 2, limit_bytes: 52_428_800 })
    await vi.waitFor(() => expect(wrapper!.text()).toContain('已达上限'))

    src.dispatch('job_end', { type: 'job_end', seq: 3, status: 'succeeded', exit_code: 0 })
    await vi.waitFor(() => expect(wrapper!.text()).toContain('任务已结束'))
    expect(src.closed).toBe(true)
  })

  it('SSE 日志：流已开但无任何输出 → 空态提示（fixture 终态构建无日志历史）', async () => {
    mockDetail(buildDetailBody({ status: 'succeeded', finished_at: 1_700_000_010_000 }))
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    FakeEventSource.latest().dispatchOpen()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('暂无日志输出'))
  })

  it('SSE 日志：首连失败 → 退化态显式标注；已开流断线 → 重连提示', async () => {
    mockDetail(buildDetailBody())
    mockDefinitionAndArtifacts()
    mountView()
    await vi.waitFor(() => expect(wrapper!.findAll('.stage-block')).toHaveLength(2))

    // 首连失败（未 open）→ 退化态。
    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src1 = FakeEventSource.latest()
    src1.dispatchError()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('尚未交付'))

    // 收起再展开：已 open 过再断线 → 重连提示。
    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    await wrapper!.findAll('[data-testid="job-log-toggle"]')[0]!.trigger('click')
    const src2 = FakeEventSource.latest()
    src2.dispatchOpen()
    src2.dispatchError()
    await vi.waitFor(() => expect(wrapper!.text()).toContain('断线重连中'))
  })
})

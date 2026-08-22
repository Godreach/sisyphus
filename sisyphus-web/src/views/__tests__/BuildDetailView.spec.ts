// 构建详情页行为测试（票 B4-T4，ADR-0006/0008/0013）：只测外部行为——
// 阶段/任务卡渲染（含 attempt 历史、排队缺失标签等待态）、触发/取消/重跑
// 的动作与 202/409 反馈、SSE 日志折叠/截断/重连。API 层以 fetch mock 驱动，
// SSE 以替身 EventSource 驱动（Spec B4 测试缝）。
//
// #93 迁移 Naive UI：阶段/任务卡改 NCard + NTag 状态徽章、触发弹窗改 NModal
// + NForm（参数覆盖 + 分支/commit）、产物下载改 NButton + 下载图标、
// 删除/取消/重跑改 NPopconfirm 确认。只测外部行为（渲染、交互、API 调用），
// 不测 Naive UI 内部实现。NModal 挂载到 body（teleport）、NPopconfirm 弹层
// 亦挂 body——断言经 document.querySelector 定位。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import BuildDetailView from '@/views/BuildDetailView.vue'
import { i18n, setLocale } from '@/i18n'
import { FakeEventSource } from '@/test/fakeEventSource'

/** 构造 mock JSON 响应。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

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
            attempt: 2,
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

/** pipeline 定义响应（排队任务缺失标签展示源）。 */
function pipelineDefBody(overrides: Record<string, unknown> = {}) {
  return {
    definition: {
      name: 'release',
      parameters: [
        { name: 'target', type: 'enum', required: true, default: 'x86_64', choices: ['x86_64', 'aarch64'] },
        { name: 'jobs', type: 'number', required: false, default: 4 },
      ],
      stages: [
        { name: 'build', jobs: [{ name: 'compile', labels: ['sisyphus/os=linux'] }] },
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
    ...overrides,
  }
}

describe('BuildDetailView（阶段/任务卡 + 触发/取消/重跑 + SSE 日志）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper
  const fetchMock = vi.fn()

  /** 按 URL 分支分发 fetch 响应（详情 / 定义 / 产物 / 动作）。
   *  注意判定顺序：/rerun、/cancel 在前（它们也含 /builds/），产物其次
   *  （.../builds/N/artifacts），详情再次，触发（POST .../builds 无号）最后
   *  落到 action 分支。 */
  function mockApi(handlers: {
    detail?: (n: number) => Record<string, unknown>
    definition?: () => Record<string, unknown>
    artifacts?: () => Record<string, unknown>
    action?: (url: string, init: RequestInit) => Record<string, unknown>
  }): void {
    fetchMock.mockImplementation((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.includes('/rerun') || url.includes('/cancel')) {
        return Promise.resolve(
          jsonResponse(202, handlers.action?.(url, init ?? {}) ?? { number: 7, build_id: 1, attempt: 2, status: 'running' }),
        )
      }
      if (url.includes('/artifacts')) {
        return Promise.resolve(jsonResponse(200, handlers.artifacts?.() ?? { items: [] }))
      }
      if (url.includes('/builds/')) {
        const num = Number(url.match(/\/builds\/(\d+)/)?.[1] ?? 7)
        return Promise.resolve(jsonResponse(200, handlers.detail?.(num) ?? buildDetailBody({ number: num })))
      }
      if (url.includes('/builds')) {
        // 触发：POST .../builds（无号）→ 202 受理。
        return Promise.resolve(
          jsonResponse(202, handlers.action?.(url, init ?? {}) ?? { number: 8, build_id: 2, attempt: 1, status: 'queued' }),
        )
      }
      if (url.includes('/pipelines/')) {
        return Promise.resolve(jsonResponse(200, handlers.definition?.() ?? pipelineDefBody()))
      }
      return Promise.resolve(jsonResponse(404, { code: 'NOT_FOUND', message: '未匹配 mock' }))
    })
  }

  /** 包装组件：直接挂载（无 useMessage 依赖，无需 NMessageProvider）。 */
  function mountView(): VueWrapper {
    wrapper = mount(BuildDetailView, {
      global: { plugins: [pinia, router, i18n] },
    })
    return wrapper
  }

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
    await router.push('/projects/demo/pipelines/release/builds/7')
    await router.isReady()
    globalThis.fetch = fetchMock
    fetchMock.mockClear()
    FakeEventSource.install()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('面包屑 + 阶段/任务卡：按快照阶段序、attempt 历史、缺失标签等待态', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 面包屑：项目 > pipeline > 构建号。
    expect(wrapper.get('.breadcrumb').text()).toContain('项目')
    expect(wrapper.get('.breadcrumb').text()).toContain('demo')
    expect(wrapper.get('.breadcrumb').text()).toContain('release')
    expect(wrapper.get('.breadcrumb').text()).toContain('构建 #7')

    // 阶段名与任务卡（含 attempt 历史：attempt=2 标注）。
    expect(wrapper.get('.stage-card').text()).toContain('build')
    expect(wrapper.findAll('.stage-card')[1]?.text()).toContain('deploy')
    expect(wrapper.text()).toContain('compile')
    expect(wrapper.text()).toContain('push')
    expect(wrapper.text()).toContain('第 2 次尝试')

    // 排队任务缺失标签等待态（ADR-0008：从定义 labels 派生）。
    const waiting = wrapper.get('.job-waiting')
    expect(waiting.text()).toContain('gpu, arch=arm64')

    // allow_failure 徽标。
    expect(wrapper.text()).toContain('允许失败')

    // 产物区：按任务声明展示 + 下载占位（缺端点退化态）。
    expect(wrapper.text()).toContain('bundle')
    expect(wrapper.text()).toContain('下载占位')

    // 状态徽章 NTag：成功任务 = 绿、排队任务 = 黄（主题 Token 色通道）。
    const jobTags = wrapper.findAll('.job-card .n-tag')
    const successTag = jobTags.find((x) => x.text() === '成功')
    expect(successTag?.attributes('style')).toContain('24, 160, 88')
  })

  it('产物区（票 #74）：已上传接下载按钮（大小/sha 提示），未上传展示占位', async () => {
    mockApi({
      artifacts: () => ({
        items: [
          {
            name: 'bundle',
            size: 4096,
            sha256: 'ab12'.repeat(16),
            created_at: 1_700_000_000_000,
          },
        ],
      }),
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.find('a.artifact-link').exists()).toBe(true))

    // 声明 bundle 已上传 → 下载按钮 + 大小；不再展示占位。
    const link = wrapper.get('a.artifact-link')
    expect(link.text()).toContain('bundle')
    expect(link.text()).toContain('4.0 KB')
    expect(link.attributes('href')).toBe(
      'api/v1/projects/demo/pipelines/release/builds/7/artifacts/bundle',
    )
    expect(link.attributes('download')).toBe('bundle')
    expect(link.attributes('title')).toContain('ab12')
    expect(wrapper.text()).not.toContain('下载占位')
  })

  it('排队等待态定义缺失时显式标注退化', async () => {
    // 定义加载 404 → 退化标注（queued 任务仍在）。
    mockApi({
      definition: () => {
        throw new Error('never')
      },
    })
    fetchMock.mockImplementation((input: RequestInfo | URL) => {
      const url = String(input)
      if (url.includes('/builds/')) {
        return Promise.resolve(jsonResponse(200, buildDetailBody()))
      }
      // 定义端点 404（定义缺失）。
      return Promise.resolve(jsonResponse(404, { code: 'NOT_FOUND', message: 'x' }))
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('退化态'))
  })

  it('触发对话框（NModal）：参数默认值预填、可覆盖，提交 POST 带参数/分支/commit', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 打开触发弹窗（第一个按钮 = 触发构建；NModal teleport 到 body）。
    await wrapper.findAll('.build-actions button')[0]?.trigger('click')
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

    // 覆盖 number 参数 jobs → 8。
    jobsInput.value = '8'
    await jobsInput.dispatchEvent(new Event('input'))

    const branchInput = document.querySelector('.n-modal input[name="trigger-branch"]') as HTMLInputElement
    const commitInput = document.querySelector('.n-modal input[name="trigger-commit"]') as HTMLInputElement
    branchInput.value = 'release/1.0'
    await branchInput.dispatchEvent(new Event('input'))
    commitInput.value = 'abc123'
    await commitInput.dispatchEvent(new Event('input'))

    const pushSpy = vi.spyOn(router, 'push')
    // 提交：点弹窗内的「触发构建」主按钮（NButton type=primary 在弹窗内）。
    const modalButtons = [...document.querySelectorAll('.n-modal button')]
    const submitBtn = modalButtons.find((b) => b.textContent?.trim() === '触发构建')
    await (submitBtn as HTMLElement).click()
    await vi.waitFor(() => expect(pushSpy).toHaveBeenCalled())

    // 请求形态：POST /builds（触发）带参数覆盖/分支/commit。
    const triggerCall = fetchMock.mock.calls.find(
      (c) => String(c[0]).match(/\/builds$/) && (c[1] as RequestInit).method === 'POST',
    ) as [string, RequestInit]
    expect(triggerCall).toBeDefined()
    const [url, init] = triggerCall
    expect(url).toContain('/api/v1/projects/demo/pipelines/release/builds')
    expect(init.method).toBe('POST')
    expect(JSON.parse(init.body as string)).toEqual({
      params: { target: 'aarch64', jobs: '8' },
      branch: 'release/1.0',
      commit: 'abc123',
    })
  })

  it('取消构建（NPopconfirm）：点击取消构建 → 弹层确认 → POST cancel，202 受理反馈', async () => {
    mockApi({
      action: () => ({ number: 7, build_id: 1, attempt: 1, status: 'running' }),
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 第二个按钮是「取消构建」（运行中可取消）→ 点开确认弹层。
    const buttons = wrapper.findAll('.build-actions button')
    await buttons[1]?.trigger('click')
    await vi.waitFor(() =>
      expect(document.querySelector('.n-popconfirm__action')).toBeTruthy(),
    )

    // 弹层确认（positive 按钮 = 第一个 action 按钮）。
    const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
    // 确认 = positive 按钮（action 末位）；首按钮为取消/关闭弹层。
    await (actionButtons[actionButtons.length - 1] as HTMLElement).click()
    await vi.waitFor(() => expect(wrapper.text()).toContain('已受理取消'))

    const cancelCall = fetchMock.mock.calls.find(
      (c) => String(c[0]).includes('/cancel'),
    ) as [string, RequestInit]
    expect(cancelCall).toBeDefined()
    expect(cancelCall[1].method).toBe('POST')
  })

  it('从失败重跑（NPopconfirm）：确认后 POST rerun（from_failed）202 受理反馈', async () => {
    mockApi({
      detail: () => buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }),
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // from_failed 重跑按钮（第 4 个）→ 点开确认弹层 → 确认。
    await wrapper.findAll('.build-actions button')[3]?.trigger('click')
    await vi.waitFor(() =>
      expect(document.querySelector('.n-popconfirm__action')).toBeTruthy(),
    )
    const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
    await (actionButtons[actionButtons.length - 1] as HTMLElement).click()
    await vi.waitFor(() => expect(wrapper.text()).toContain('已受理重跑'))
    const rerunCall = fetchMock.mock.calls.find(
      (c) => String(c[0]).includes('/rerun'),
    ) as [string, RequestInit]
    expect(rerunCall).toBeDefined()
    expect(JSON.parse(rerunCall[1].body as string)).toEqual({ mode: 'from_failed' })
  })

  it('从失败重跑：非失败终态 409 拒绝反馈', async () => {
    // rerun 端点返回 409（非 failed/cancelled/timeout 终态）。
    fetchMock.mockImplementation((input: RequestInfo | URL) => {
      const url = String(input)
      if (url.includes('/rerun')) {
        return Promise.resolve(
          jsonResponse(409, {
            code: 'CONFLICT',
            message: '构建当前状态不可从失败重跑（仅 failed/cancelled/timeout 终态可重跑）',
          }),
        )
      }
      if (url.includes('/builds/')) {
        return Promise.resolve(jsonResponse(200, buildDetailBody({ status: 'running' })))
      }
      if (url.includes('/pipelines/')) {
        return Promise.resolve(jsonResponse(200, pipelineDefBody()))
      }
      return Promise.resolve(jsonResponse(404, { code: 'NOT_FOUND', message: 'x' }))
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))
    await wrapper.findAll('.build-actions button')[3]?.trigger('click')
    await vi.waitFor(() =>
      expect(document.querySelector('.n-popconfirm__action')).toBeTruthy(),
    )
    const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
    await (actionButtons[actionButtons.length - 1] as HTMLElement).click()
    await vi.waitFor(() => expect(wrapper.text()).toContain('无法从失败重跑'))
  })

  it('删除构建（票 #78，NPopconfirm）：终态可删、确认后发 DELETE、204 后跳回构建列表', async () => {
    mockApi({
      detail: () => buildDetailBody({ status: 'failed', finished_at: 1_700_000_010_000 }),
    })
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 终态构建：删除按钮可用（运行中/排队禁用由 isLiveStatus 反向控制）。
    const deleteBtn = wrapper.findAll('.build-actions button').find((b) => b.text() === '删除构建')
    expect(deleteBtn).toBeDefined()
    expect(deleteBtn?.attributes('disabled')).toBeUndefined()

    // 打开确认弹层 → 点弹层外「取消」（negative 按钮）→ 不发请求。
    await deleteBtn?.trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
    const actionButtons = document.querySelectorAll('.n-popconfirm__action button')
    // 取消确认 = negative 按钮（action 首位）。
    await (actionButtons[0] as HTMLElement).click()
    await new Promise((r) => setTimeout(r, 50))
    expect(
      fetchMock.mock.calls.some((c) => String(c[0]).includes('/builds/7') && (c[1] as RequestInit).method === 'DELETE'),
    ).toBe(false)

    // 再次打开并确认（positive）→ DELETE 204 → 跳回构建列表。
    const pushSpy = vi.spyOn(router, 'push')
    await wrapper.findAll('.build-actions button').find((b) => b.text() === '删除构建')?.trigger('click')
    await vi.waitFor(() => expect(document.querySelector('.n-popconfirm__action')).toBeTruthy())
    const actionButtons2 = document.querySelectorAll('.n-popconfirm__action button')
    await (actionButtons2[actionButtons2.length - 1] as HTMLElement).click()
    await vi.waitFor(() => expect(pushSpy).toHaveBeenCalled())

    const delCall = fetchMock.mock.calls.find(
      (c) => String(c[0]).includes('/builds/7') && (c[1] as RequestInit).method === 'DELETE',
    ) as [string, RequestInit] | undefined
    expect(delCall).toBeDefined()
    expect(delCall?.[0]).toContain('/api/v1/projects/demo/pipelines/release/builds/7')
    expect(pushSpy.mock.calls[0]?.[0]).toMatchObject({ name: 'build-list' })
  })

  it('删除构建（票 #78）：运行中构建禁用删除按钮', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 运行中（默认 detail 即 running）：删除按钮禁用。
    const deleteBtn = wrapper.findAll('.build-actions button').find((b) => b.text() === '删除构建')
    expect(deleteBtn?.attributes('disabled')).toBeDefined()
  })

  it('SSE 日志：查看日志展开流，输出块合流渲染、步骤折叠/展开', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 展开 compile 任务日志。
    await wrapper.findAll('.job-log-toggle')[0]?.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src = FakeEventSource.latest()
    expect(src.url).toContain('/builds/7/jobs/compile/attempts/1/logs/stream?from=0')

    src.dispatchOpen()
    src.dispatch('step_start', {
      type: 'step_start', seq: 1, step: 0, name: 'build', command: 'cargo build', started_at: 1,
    })
    src.dispatch('output', { type: 'output', seq: 2, stream: 'stdout', text: 'compiling...\x1b[32mok\x1b[0m' })
    src.dispatch('output', { type: 'output', seq: 3, stream: 'stderr', text: 'warning' })
    await vi.waitFor(() => expect(wrapper.text()).toContain('cargo build'))

    // ANSI 剥离：渲染文本无色码（ADR-0013）。
    expect(wrapper.text()).toContain('compiling...ok')
    expect(wrapper.text()).not.toContain('\x1b[32m')

    // 步骤折叠：点击步骤头 → 输出隐藏。
    await wrapper.get('.build-log-step-head').trigger('click')
    expect(wrapper.find('.build-log-step-body').exists()).toBe(false)

    // 再展开。
    await wrapper.get('.build-log-step-head').trigger('click')
    expect(wrapper.find('.build-log-step-body').exists()).toBe(true)
  })

  it('SSE 日志：截断显著标注、终态事件送达即关流', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    await wrapper.findAll('.job-log-toggle')[0]?.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src = FakeEventSource.latest()

    src.dispatchOpen()
    src.dispatch('output', { type: 'output', seq: 1, stream: 'stdout', text: 'some log' })
    src.dispatch('truncated', { type: 'truncated', seq: 2, limit_bytes: 52_428_800 })
    await vi.waitFor(() => expect(wrapper.text()).toContain('已达上限'))

    src.dispatch('job_end', { type: 'job_end', seq: 3, status: 'succeeded', exit_code: 0 })
    await vi.waitFor(() => expect(wrapper.text()).toContain('任务已结束'))
    expect(src.closed).toBe(true)
  })

  it('SSE 日志：首连失败 → 退化态显式标注；已开流断线 → 重连提示', async () => {
    mockApi({})
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-card')).toHaveLength(2))

    // 首连失败（未 open）→ 退化态。
    await wrapper.findAll('.job-log-toggle')[0]?.trigger('click')
    await vi.waitFor(() => expect(FakeEventSource.instances.length).toBeGreaterThan(0))
    const src1 = FakeEventSource.latest()
    src1.dispatchError()
    await vi.waitFor(() => expect(wrapper.text()).toContain('尚未交付'))

    // 收起再展开：已 open 过再断线 → 重连提示。
    await wrapper.findAll('.job-log-toggle')[0]?.trigger('click')
    await wrapper.findAll('.job-log-toggle')[0]?.trigger('click')
    const src2 = FakeEventSource.latest()
    src2.dispatchOpen()
    src2.dispatchError()
    await vi.waitFor(() => expect(wrapper.text()).toContain('断线重连中'))
  })
})

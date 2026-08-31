// 混合式 pipeline 编辑器行为测试（票 B4-T8，ADR-0020 变体 C；票 #109 迁移
// MSW 单一缝——组件经真实 http client 打 src/mocks handlers，per-test 用
// server.use 覆盖定义端点响应；请求形态断言经覆盖 handler 内捕获）。
//
// 只测外部行为：轨道导航表单、字段联动、增删/重排、保存校验整组展示 + 字段定位、
// PUT 原样提交 + revision、并发冲突弹窗、服务端 422、404 空定义、参数/环境变量页签。
// 定义端点本身的契约（档位守卫/422 形态/revision 语义/往返）对账另见
// src/mocks/__tests__/pipelinesContract.spec.ts。
//
// 关键：mount 须在 handler 覆盖之后（onMounted 即发 GET 定义）。
// #96: 编辑器迁移 Naive UI——chip 改 NTag（update:checked 驱动选中）、数字输入改
// NInputNumber、开关改 NSwitch、页签改 NTabs、成功 toast / 冲突弹窗 teleport 不在
// wrapper 内——toast 文案与 NModal 弹层经 document 断言（同 UsersView.spec 纪律）；
// NSelect 经 findComponent + $emit 驱动（无原生 select，同 SecretsView.spec）。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider, NInputNumber, NSelect } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import PipelineEditorView from '@/views/PipelineEditorView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

const DEF_URL = '/api/v1/projects/proj-a/pipelines/main'

/** GET 定义响应（model Pipeline JSON + 顶层 revision/operator/updated_at）。 */
function defResp(revision = 1, definition?: unknown) {
  return {
    definition: definition ?? {
      name: 'main',
      parameters: [],
      env: [],
      stages: [
        {
          name: 'build',
          jobs: [
            { name: 'compile', steps: [] },
            { name: 'test', steps: [] },
          ],
        },
      ],
    },
    revision,
    operator: 'alice',
    updated_at: 1700000000000,
  }
}

/** PUT 保存响应（新 revision）。 */
function saveResp(revision: number) {
  return { revision, operator: 'alice', updated_at: 1700000000001 }
}

/** 覆盖 GET 定义端点（记录命中次数——「重新加载再拉一次」类断言用）。 */
let getCalls = 0
function mockDefinition(body: Record<string, unknown>, status = 200): void {
  server.use(
    http.get(DEF_URL, () => {
      getCalls += 1
      return HttpResponse.json(body, { status })
    }),
  )
}

/** 覆盖 PUT 保存端点（捕获请求体——PUT 形态断言 + 「未提交」断言共用）。 */
let putBodies: unknown[]
function mockSave(
  impl: (body: unknown) => { body: Record<string, unknown>; status?: number },
): void {
  server.use(
    http.put(DEF_URL, async ({ request }) => {
      const body = await request.json()
      putBodies.push(body)
      const r = impl(body)
      return HttpResponse.json(r.body, { status: r.status ?? 200 })
    }),
  )
}

/** 包装组件：NMessageProvider + 编辑器，保证 useMessage 注入可用。 */
const EditorWrapper = defineComponent({
  name: 'EditorWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(PipelineEditorView, { ...attrs }))
  },
})

describe('PipelineEditorView 混合式编辑器', () => {
  let pinia: Pinia
  let router: Router

  function mountView(): VueWrapper {
    return mount(EditorWrapper, { global: { plugins: [pinia, router, i18n] } })
  }

  /** 切换 NTabs 页签（按标签文本点击 tab）。 */
  async function switchTab(w: VueWrapper, label: string): Promise<void> {
    const tab = w.findAll('.n-tabs-tab').find((el) => el.text() === label)
    expect(tab, `页签 ${label}`).toBeTruthy()
    await tab!.trigger('click')
  }

  /** 按 name 定位 NSelect 并以 update:value 事件驱动（无原生 select）。 */
  async function selectByNName(w: VueWrapper, name: string, value: string): Promise<void> {
    const sel = w.findAllComponents(NSelect).find((c) => c.attributes('name') === name)
    expect(sel, `NSelect ${name}`).toBeTruthy()
    await sel!.vm.$emit('update:value', value)
  }

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    getCalls = 0
    putBodies = []
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        // 面包屑链接目标（不导航，仅占位防 unknown-name 警告）。
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        {
          path: '/projects/:name/detail',
          name: 'project-detail',
          component: { template: '<div />' },
        },
        {
          path: '/projects/:name/pipelines/:pipeline',
          name: 'pipeline-edit',
          component: PipelineEditorView,
        },
      ],
    })
    await router.push('/projects/proj-a/pipelines/main')
    await router.isReady()
  })

  afterEach(() => {
    vi.restoreAllMocks()
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('加载定义 → 轨道阶段列 + 任务 chip，表单选中首任务', async () => {
    mockDefinition(defResp())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-column').length).toBe(1))

    // 两个任务 chip（NTag）；首任务自动选中，表单名称字段 = compile。
    expect(wrapper.findAll('.job-chip').length).toBe(2)
    await vi.waitFor(() =>
      expect((wrapper.find('input[name="job-name"]').element as HTMLInputElement).value).toBe(
        'compile',
      ),
    )
    wrapper.unmount()
  })

  it('点击 chip → 表单导航到该任务（字段联动）+ 选中态高亮', async () => {
    mockDefinition(defResp())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 点击第二个 chip → 表单切换到 test。
    await wrapper.find('[name="chip-0-1"]').trigger('click')
    await vi.waitFor(() =>
      expect((wrapper.find('input[name="job-name"]').element as HTMLInputElement).value).toBe(
        'test',
      ),
    )
    // 选中态落在第二个 chip（NTag checked + chip-selected 类）。
    expect(wrapper.find('[name="chip-0-1"]').classes()).toContain('chip-selected')
    expect(wrapper.find('[name="chip-0-1"]').classes()).toContain('n-tag--checked')
    wrapper.unmount()
  })

  it('表单字段编辑 → 轨道 chip 联动（名称 / 重试 / allow-failure 角标）', async () => {
    mockDefinition(defResp())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 改任务名 → chip 名称更新。
    await wrapper.find('input[name="job-name"]').setValue('renamed')
    await vi.waitFor(() =>
      expect(wrapper.find('[name="chip-0-0"]').text()).toContain('renamed'),
    )

    // 设重试次数（NInputNumber——原生 input 事件不提交值，经组件 emit 驱动；
    // 表单内首个即重试，第二个为超时）→ chip 出现「重试 2」角标。
    const retryInput = wrapper.findAllComponents(NInputNumber)[0]!
    expect(retryInput, 'NInputNumber 重试').toBeTruthy()
    await retryInput.vm.$emit('update:value', 2)
    await vi.waitFor(() =>
      expect(wrapper.find('[name="chip-0-0"]').text()).toContain('重试 2'),
    )

    // 勾选 allow_failure（NSwitch）→ chip 出现角标。
    await wrapper.find('[name="job-allow-failure"]').trigger('click')
    await vi.waitFor(() =>
      expect(wrapper.find('[name="chip-0-0"]').text()).toContain('允许失败'),
    )
    wrapper.unmount()
  })

  it('增删/重排阶段与任务', async () => {
    mockDefinition(defResp())

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.stage-column').length).toBe(1))

    // 新增阶段 → 2 列。
    await wrapper.find('[name="track-add-stage"]').trigger('click')
    expect(wrapper.findAll('.stage-column').length).toBe(2)

    // 在首阶段新增任务 → 3 chip。
    await wrapper.find('[name="stage-0-add-job"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(3))

    // 删除首阶段首个任务 → 2 chip。
    await wrapper.find('[name="job-0-0-delete"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 上移第二阶段（无可视序断言，确保不崩 + 按钮可点）。
    await wrapper.find('[name="stage-1-up"]').trigger('click')
    expect(wrapper.findAll('.stage-column').length).toBe(2)
    wrapper.unmount()
  })

  it('保存校验：本地拦下 + 整组展示 + 字段路径定位 + 不提交', async () => {
    mockDefinition(defResp())
    // PUT 若被误调用会记录 putBodies（断言不应到达）。
    mockSave(() => ({ body: saveResp(2) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 选中任务加一个 shell 步骤（空命令 → shell_command_empty）。
    await wrapper.find('[name="job-step-add-shell"]').trigger('click')

    // 保存 → 本地校验拦下，错误面板含字段路径，chip 红边，未发 PUT。
    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('.editor-errors').exists()).toBe(true))
    expect(wrapper.find('.editor-errors').text()).toContain(
      'stages[0].jobs[0].steps[0].command',
    )
    expect(wrapper.find('[name="chip-0-0"]').classes()).toContain('chip-error')
    // NForm validation 模式：步骤命令字段的 NFormItem feedback 就近红显同一错误。
    expect(wrapper.find('.n-form-item-feedback').text()).toContain(
      'stages[0].jobs[0].steps[0].command',
    )
    expect(putBodies.length).toBe(0)
    wrapper.unmount()
  })

  it('保存合法定义 → PUT 原样提交 model JSON（剥离 revision）+ 成功 toast + revision', async () => {
    mockDefinition(defResp(1))
    mockSave(() => ({ body: saveResp(2) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    await wrapper.find('[name="editor-save"]').trigger('click')
    // 成功 toast teleport 到 body（NMessage）。
    await vi.waitFor(() => expect(document.body.textContent).toContain('已保存'))

    // PUT 形态：原样 definition（无 revision）。
    expect(putBodies.length).toBe(1)
    expect(putBodies[0]).toEqual(defResp(1).definition)

    // 成功回执含新 revision（toast）；revision 区更新为新版本。
    expect(document.body.textContent).toContain('revision 2')
    expect(wrapper.find('.editor-rev-value').text()).toBe('2')
    wrapper.unmount()
  })

  it('并发保存冲突：加载 revision 3，PUT 返回 revision 5 → 冲突弹窗（可重新加载）', async () => {
    mockDefinition(defResp(3))
    mockSave(() => ({ body: saveResp(5) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    await wrapper.find('[name="editor-save"]').trigger('click')
    // 冲突 NModal teleport 到 body。
    await vi.waitFor(() => expect(document.querySelector('.n-modal')).toBeTruthy())
    const msg = document.querySelector('.n-modal')?.textContent ?? ''
    expect(msg).toContain('revision 3')
    expect(msg).toContain('revision 4')
    expect(msg).toContain('revision 5')

    // 弹窗内「重新加载」→ 重新 GET 定义（命中计数 +1）。
    const getsBefore = getCalls
    await (document.querySelector('.n-modal button[name="conflict-reload"]') as HTMLElement).click()
    await vi.waitFor(() => expect(getCalls).toBe(getsBefore + 1))
    wrapper.unmount()
  })

  it('服务端 422 → 服务端校验清单整组展示（字段定位）', async () => {
    mockDefinition(defResp())
    mockSave(() => ({
      status: 422,
      body: {
        code: 'VALIDATION_FAILED',
        message: 'model 校验失败',
        detail: {
          errors: [{ path: 'parameters[0].target.required', message: '必填参数必须带默认值' }],
        },
      },
    }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 合法定义本地通过 → 发 PUT → 服务端 422 清单展示。
    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('.editor-errors').exists()).toBe(true))
    expect(wrapper.find('.editor-errors').text()).toContain('服务端校验')
    expect(wrapper.find('.editor-errors').text()).toContain('parameters[0].target.required')
    wrapper.unmount()
  })

  it('GET 404 → 空定义开始（未保存），新增阶段可用', async () => {
    mockDefinition({ code: 'NOT_FOUND', message: 'pipeline 不存在' }, 404)

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('未保存'))
    expect(wrapper.text()).toContain('空定义')
    expect(wrapper.findAll('.stage-column').length).toBe(0)

    // 空定义也可新增阶段。
    await wrapper.find('[name="track-add-stage"]').trigger('click')
    expect(wrapper.findAll('.stage-column').length).toBe(1)
    wrapper.unmount()
  })

  it('参数页签：四类型 + 必填带默认值校验 + enum 候选项', async () => {
    mockDefinition(defResp())
    mockSave(() => ({ body: saveResp(2) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 切到参数页签 + 新增参数。
    await switchTab(wrapper, '参数')
    await wrapper.find('[name="param-add"]').trigger('click')
    expect(wrapper.findAll('.param-card').length).toBe(1)

    // 类型切到 enum（NSelect）→ 候选项 textarea 出现。
    await selectByNName(wrapper, 'param-0-type', 'enum')
    expect(wrapper.find('textarea[name="param-0-choices"]').exists()).toBe(true)

    // 切到 number + 必填（NSwitch）+ 清默认 → 保存触发 R1（必填参数必须带默认值）。
    await selectByNName(wrapper, 'param-0-type', 'number')
    await wrapper.find('input[name="param-0-name"]').setValue('target')
    await wrapper.find('[name="param-0-required"]').trigger('click')
    // 默认值字段保持空（= undefined）。
    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('.editor-errors').exists()).toBe(true))
    expect(wrapper.find('.editor-errors').text()).toContain('parameters[0].target.required')
    expect(putBodies.length).toBe(0)
    wrapper.unmount()
  })

  it('环境变量页签：增改后保存 → PUT 体含 env', async () => {
    mockDefinition(defResp())
    mockSave(() => ({ body: saveResp(2) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    await switchTab(wrapper, '环境变量')
    await wrapper.find('[name="pipe-env-add"]').trigger('click')
    await wrapper.find('input[name="pipe-env-0-name"]').setValue('RUST_LOG')
    await wrapper.find('input[name="pipe-env-0-value"]').setValue('debug')

    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(document.body.textContent).toContain('已保存'))

    const body = putBodies[0] as { env: { name: string; value: string }[] }
    expect(body.env).toEqual([{ name: 'RUST_LOG', value: 'debug' }])
    wrapper.unmount()
  })

  it('任务级 env：job.env 初始 undefined，增改后落回 job 并随保存提交（回归）', async () => {
    // defResp 的任务无 env 字段（undefined）——验证 EnvListEditor 的懒初始化
    // 把 job.env 钉成响应式数组、add 的 push 落回 job.env（评审实测的硬 bug）。
    mockDefinition(defResp())
    mockSave(() => ({ body: saveResp(2) }))

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))

    // 任务页签 + 选中首任务（job.env 缺省 undefined）。
    await switchTab(wrapper, '任务')
    expect(wrapper.find('input[name="job-env-0-name"]').exists()).toBe(false)

    // 新增任务级 env + 填值 → 行渲染（push 落回 job.env）。
    await wrapper.find('[name="job-env-add"]').trigger('click')
    await wrapper.find('input[name="job-env-0-name"]').setValue('DEBUG')
    await wrapper.find('input[name="job-env-0-value"]').setValue('1')

    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(document.body.textContent).toContain('已保存'))

    // PUT 体含任务级 env（落回 job，非临时数组）。
    const body = putBodies[0] as { stages: { jobs: { env: unknown }[] }[] }
    expect(body.stages[0]!.jobs[0]!.env).toEqual([{ name: 'DEBUG', value: '1' }])
    wrapper.unmount()
  })

  // ---- 票 #109 增补：编辑器直消费 mock fixture（不覆盖端点）的真闭环 ----

  it('mock fixture（web-app/main）：model 形态定义全字段渲染 + 真 mock PUT 往返（revision 3→4 持久化）', async () => {
    // 不覆盖端点——直接消费 mock db fixture（防「fixture decl 形态漂移编辑器渲染
    // 全挂」回归；fixture 即测试数据，单一缝纪律）。
    await router.push('/projects/web-app/pipelines/main')
    await router.isReady()

    const wrapper = mountView()
    // fixture：build（compile/unit-test）→ check（lint/audit）两阶段四任务。
    await vi.waitFor(() => expect(wrapper.findAll('.stage-column').length).toBe(2))
    expect(wrapper.findAll('.job-chip').length).toBe(4)
    await vi.waitFor(() =>
      expect((wrapper.find('input[name="job-name"]').element as HTMLInputElement).value).toBe(
        'compile',
      ),
    )
    // fixture 步骤（shell 型 tagged union）渲染——命令字段有值。
    expect(
      (wrapper.find('textarea[name="job-step-0-command"]').element as HTMLTextAreaElement).value
        .length,
    ).toBeGreaterThan(0)

    // 保存走真 mock PUT（overlay 持久化）：fixture 基线 revision 3 → 4。
    await wrapper.find('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(document.body.textContent).toContain('已保存'))
    await vi.waitFor(() => expect(wrapper.find('.editor-rev-value').text()).toBe('4'))

    // 重载读回 overlay：revision 保持 4（保存→重载往返闭环）。
    await wrapper.find('[name="editor-reload"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('.editor-rev-value').text()).toBe('4'))
    wrapper.unmount()
  })

  it('双语切换不炸布局：en-US 下三页签/表单/轨道完整渲染（表单标签按最长语言设计）', async () => {
    mockDefinition(defResp())
    setLocale('en-US')

    const wrapper = mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.job-chip').length).toBe(2))
    // 任务页签（en）：轨道 + 表单并排均在，表单首字段标签为英文。
    expect(wrapper.find('.editor-jobs-pane').exists()).toBe(true)
    expect(wrapper.find('label').text()).toContain('Job name')
    // 三页签 en 标签齐全可切换。
    await switchTab(wrapper, 'Parameters')
    await switchTab(wrapper, 'Env vars')
    await switchTab(wrapper, 'Jobs')
    wrapper.unmount()
  })
})

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NSelect } from 'naive-ui'
import { http, HttpResponse } from 'msw'

import App from '@/App.vue'
import PipelinesView from '@/views/PipelinesView.vue'
import ProjectDetailView from '@/views/ProjectDetailView.vue'
import PipelineEditorView from '@/views/PipelineEditorView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'
import { useAuthStore } from '@/stores/auth'
import { pipelinesApi } from '@/api/client'

describe('新建流水线真实页面闭环（#120）', () => {
  let wrapper: VueWrapper
  let router: Router
  let writes: Request[]

  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
  beforeEach(() => {
    setLocale('zh-CN')
    vi.spyOn(window, 'scrollTo').mockImplementation(() => {})
    writes = []
    server.events.on('request:start', ({ request }) => {
      if (!['GET', 'HEAD'].includes(request.method)) writes.push(request)
    })
  })
  afterEach(() => {
    wrapper?.unmount()
    server.events.removeAllListeners()
    server.resetHandlers()
    vi.restoreAllMocks()
  })
  afterAll(() => server.close())

  async function mountAt(source: string, isAdmin = true): Promise<void> {
    const pinia = createPinia()
    setActivePinia(pinia)
    useAuthStore().setAuthed({ username: isAdmin ? 'admin' : 'alice', isAdmin })
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/login', name: 'login', component: { template: '<div />' } },
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/pipelines', name: 'pipelines', component: PipelinesView },
        { path: '/projects/:name', name: 'project-detail', component: ProjectDetailView },
        { path: '/projects/:name/pipelines/:pipeline', name: 'pipeline-edit', component: PipelineEditorView },
        { path: '/projects/:name/pipelines/:pipeline/builds', name: 'build-list', component: { template: '<div />' } },
        { path: '/projects/:name/pipelines/:pipeline/builds/:number', name: 'build-detail', component: { template: '<div />' } },
        { path: '/:pathMatch(.*)*', name: 'not-found', component: { template: '<div />' } },
      ],
    })
    await router.push('/')
    await router.push(source)
    await router.isReady()
    wrapper = mount(App, { attachTo: document.body, global: { plugins: [pinia, router, i18n] } })
  }

  async function enterDraft(selector: string, name: string): Promise<void> {
    await vi.waitFor(() => expect(wrapper.find(selector).exists()).toBe(true))
    await wrapper.get(selector).trigger('click')
    await vi.waitFor(() => expect(document.querySelector('input[name="new-pipeline-name"]')).toBeTruthy())
    if (document.querySelector('[data-testid="new-pipeline-project"]')) {
      const select = wrapper.findAllComponents(NSelect).find(c => c.attributes('data-testid') === 'new-pipeline-project')!
      await select.vm.$emit('update:value', 'web-app')
    } else {
      expect(document.querySelector('[data-testid="new-pipeline-project-locked"]')?.textContent).toContain('web-app')
    }
    const input = document.querySelector('input[name="new-pipeline-name"]') as HTMLInputElement
    input.value = name
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await wrapper.vm.$nextTick()
    ;(document.querySelector('[data-testid="new-pipeline-create"]') as HTMLElement).click()
    await vi.waitFor(() => expect(wrapper.find('[data-testid="editor-new-badge"]').exists()).toBe(true))
    expect(router.currentRoute.value.query.create).toBe('1')
    expect(writes).toHaveLength(0)
  }

  it.each([
    ['/pipelines?group=flat', '[data-testid="topbar-cta"]', 'flow-global'],
    ['/projects/web-app?tab=pipelines', '[data-testid="new-pipeline-btn"]', 'flow-project'],
    ['/projects/web-app?tab=pipelines', '[data-testid="pipeline-empty-create-btn"]', 'flow-empty'],
  ])('%s 的真实入口 %s 首存创建、续存和来源返回', async (source, selector, name) => {
    if (name === 'flow-empty') {
      server.use(http.get('/api/v1/pipelines', () => HttpResponse.json({ items: [], total: 0 })))
    }
    await mountAt(source)
    await enterDraft(selector, name)
    expect((await pipelinesApi.list()).items.some(p => p.pipeline === name)).toBe(false)
    await wrapper.get('[name="track-add-stage"]').trigger('click')
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.get('.editor-rev-value').text()).toBe('1'))
    expect(writes[0]!.headers.get('If-None-Match')).toBe('*')
    expect(wrapper.get('[name="editor-save"]').text()).toBe('保存')
    expect(router.currentRoute.value.query.create).toBeUndefined()
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.get('.editor-rev-value').text()).toBe('2'))
    expect(writes[1]!.headers.has('If-None-Match')).toBe(false)
    const unload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(unload)
    expect(unload.defaultPrevented).toBe(false)
    router.back()
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe(source))
    expect(router.currentRoute.value.query.create).toBeUndefined()
  })

  it('脏草稿路由离开、浏览器返回和刷新保护；继续或放弃由用户选择', async () => {
    await mountAt('/pipelines?group=flat')
    await enterDraft('[data-testid="topbar-cta"]', 'flow-dirty')
    const cleanUnload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(cleanUnload)
    expect(cleanUnload.defaultPrevented).toBe(false)
    await wrapper.get('[name="track-add-stage"]').trigger('click')
    const dirtyUnload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(dirtyUnload)
    expect(dirtyUnload.defaultPrevented).toBe(true)
    await router.push('/projects')
    await vi.waitFor(() => expect(document.querySelector('[data-testid="unsaved-continue"]')).toBeTruthy())
    ;(document.querySelector('[data-testid="unsaved-continue"]') as HTMLElement).click()
    expect(router.currentRoute.value.name).toBe('pipeline-edit')
    router.back()
    await vi.waitFor(() => expect(document.querySelector('[data-testid="unsaved-discard"]')).toBeTruthy())
    ;(document.querySelector('[data-testid="unsaved-discard"]') as HTMLElement).click()
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/pipelines?group=flat'))
  })

  it('同名预检留在对话框，只有主动打开才进入已有流水线且不写入', async () => {
    await mountAt('/pipelines?group=flat&q=main')
    await vi.waitFor(() => expect(wrapper.find('[data-testid="topbar-cta"]').exists()).toBe(true))
    await wrapper.get('[data-testid="topbar-cta"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('input[name="new-pipeline-name"]')).toBeTruthy())
    const select = wrapper.findAllComponents(NSelect).find(c => c.attributes('data-testid') === 'new-pipeline-project')!
    await select.vm.$emit('update:value', 'web-app')
    const input = document.querySelector('input[name="new-pipeline-name"]') as HTMLInputElement
    input.value = 'main'
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await wrapper.vm.$nextTick()
    ;(document.querySelector('[data-testid="new-pipeline-create"]') as HTMLElement).click()
    await vi.waitFor(() => expect(document.querySelector('[data-testid="new-pipeline-name-error"]')?.textContent).toContain('同名'))
    expect(router.currentRoute.value.name).toBe('pipelines')
    const open = document.querySelector('[data-testid="new-pipeline-open-existing"]') as HTMLButtonElement
    expect(open).toBeTruthy()
    open.click()
    await vi.waitFor(() => expect(wrapper.find('[name="editor-save"]').exists()).toBe(true))
    expect(router.currentRoute.value.params.pipeline).toBe('main')
    expect(router.currentRoute.value.query.create).toBeUndefined()
    expect(wrapper.find('[data-testid="editor-new-badge"]').exists()).toBe(false)
    expect(writes).toHaveLength(0)
    router.back()
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/pipelines?group=flat&q=main'))
  })

  it('创建主操作要求明确选择项目和非空名称，Enter 仍显示就地校验', async () => {
    await mountAt('/pipelines?create=1')
    await vi.waitFor(() => expect(document.querySelector('input[name="new-pipeline-name"]')).toBeTruthy())
    const create = document.querySelector('[data-testid="new-pipeline-create"]') as HTMLButtonElement
    const input = document.querySelector('input[name="new-pipeline-name"]') as HTMLInputElement
    expect(create.disabled).toBe(true)
    input.value = ' 中文流水线 '
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await wrapper.vm.$nextTick()
    expect(create.disabled).toBe(true)
    input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', bubbles: true }))
    await vi.waitFor(() => expect(document.querySelector('[data-testid="new-pipeline-project-error"]')).toBeTruthy())
    const select = wrapper.findAllComponents(NSelect).find(c => c.attributes('data-testid') === 'new-pipeline-project')!
    await select.vm.$emit('update:value', 'web-app')
    await vi.waitFor(() => expect(create.disabled).toBe(false))
    input.value = '   '
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await wrapper.vm.$nextTick()
    expect(create.disabled).toBe(true)
    input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', bubbles: true }))
    await vi.waitFor(() => expect(document.querySelector('[data-testid="new-pipeline-name-error"]')?.textContent).toContain('请输入流水线名'))
    expect(writes).toHaveLength(0)
  })

  it('预检期间浏览器后退关闭对话框，迟到响应不能跳进编辑器', async () => {
    let finishCheck!: () => void
    let checkStarted = false
    let checkFinished = false
    server.use(http.get('/api/v1/projects/web-app/pipelines/flow-cancel-check', async () => {
      checkStarted = true
      await new Promise<void>(resolve => { finishCheck = resolve })
      return HttpResponse.json({ code: 'NOT_FOUND' }, { status: 404 })
    }))
    server.events.on('request:end', ({ request }) => {
      if (request.url.endsWith('/pipelines/flow-cancel-check')) checkFinished = true
    })
    await mountAt('/pipelines?group=flat')
    await vi.waitFor(() => expect(wrapper.find('[data-testid="topbar-cta"]').exists()).toBe(true))
    await wrapper.get('[data-testid="topbar-cta"]').trigger('click')
    await vi.waitFor(() => expect(document.querySelector('input[name="new-pipeline-name"]')).toBeTruthy())
    const select = wrapper.findAllComponents(NSelect).find(c => c.attributes('data-testid') === 'new-pipeline-project')!
    await select.vm.$emit('update:value', 'web-app')
    const input = document.querySelector('input[name="new-pipeline-name"]') as HTMLInputElement
    input.value = 'flow-cancel-check'
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await wrapper.vm.$nextTick()
    ;(document.querySelector('[data-testid="new-pipeline-create"]') as HTMLElement).click()
    await vi.waitFor(() => expect(checkStarted).toBe(true))
    router.back()
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/pipelines?group=flat'))
    finishCheck()
    await vi.waitFor(() => expect(checkFinished).toBe(true))
    // Wait through the full HTTP/router turn, not merely handler completion.
    await new Promise(resolve => setTimeout(resolve, 100))
    expect(router.currentRoute.value.fullPath).toBe('/pipelines?group=flat')
    expect(writes).toHaveLength(0)
  })

  it.each(['explicit', 'browser'])('草稿 %s 返回恢复状态筛选和列表视图，不重开对话框', async action => {
    await mountAt('/pipelines?group=flat')
    await vi.waitFor(() => expect(wrapper.find('[data-testid="chip-success"]').exists()).toBe(true))
    await wrapper.get('[data-testid="chip-success"]').trigger('click')
    await vi.waitFor(() => expect(router.currentRoute.value.query.status).toBe('success'))
    await wrapper.get('[data-testid="view-list-btn"]').trigger('click')
    await vi.waitFor(() => expect(router.currentRoute.value.query.view).toBe('list'))
    const source = router.currentRoute.value.fullPath
    await enterDraft('[data-testid="topbar-cta"]', `flow-return-${action}`)
    if (action === 'explicit') await wrapper.get('[data-testid="editor-back"]').trigger('click')
    else router.back()
    await vi.waitFor(() => expect(wrapper.find('[data-testid="chip-success"]').exists()).toBe(true))
    expect(wrapper.get('[data-testid="chip-success"]').classes()).toContain('active')
    expect(wrapper.get('[data-testid="view-list-btn"]').classes()).toContain('active')
    expect(router.currentRoute.value.fullPath).toBe(source)
    expect(router.currentRoute.value.query.create).toBeUndefined()
  })

  it.each(['conflict', 'load-error'])('新建链接 %s 状态仍有显式来源返回入口', async state => {
    if (state === 'load-error') {
      server.use(http.get('/api/v1/projects/web-app/pipelines/main', () => HttpResponse.json({ code: 'INTERNAL', message: '暂不可用' }, { status: 500 })))
    }
    await mountAt('/projects/web-app/pipelines/main?create=1&from=/pipelines?group=flat')
    await vi.waitFor(() => expect(wrapper.find('[role="alert"]').exists()).toBe(true))
    expect(wrapper.find('[data-testid="editor-back"]').exists()).toBe(true)
    await wrapper.get('[data-testid="editor-back"]').trigger('click')
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/pipelines?group=flat'))
    expect(writes).toHaveLength(0)
  })

  it('普通用户无可管理项目时隐藏全局入口并明确提示联系管理员', async () => {
    server.use(http.get('/api/v1/projects', () => HttpResponse.json([])))
    await mountAt('/pipelines', false)
    await vi.waitFor(() => expect(wrapper.find('[data-testid="pipeline-create-unavailable"]').exists()).toBe(true))
    expect(wrapper.find('[data-testid="topbar-cta"]').exists()).toBe(false)
    expect(wrapper.get('[data-testid="pipeline-create-unavailable"]').text()).toContain('请联系项目管理员')
  })

  it('项目内入口使用管理清单，成员接口失败不剥夺创建权限', async () => {
    server.use(http.get('/api/v1/projects/web-app/members', () => HttpResponse.json({ code: 'INTERNAL' }, { status: 500 })))
    await mountAt('/projects/web-app', false)
    await vi.waitFor(() => expect(wrapper.find('[data-testid="project-title"]').exists()).toBe(true))
    await vi.waitFor(() => expect(wrapper.find('[data-testid="new-pipeline-btn"]').exists()).toBe(true))
    await enterDraft('[data-testid="new-pipeline-btn"]', 'flow-independent-permission')
    expect(router.currentRoute.value.params.name).toBe('web-app')
  })

  it.each(['/pipelines', '/projects/web-app'])('%s 创建权限加载失败不误报无权限，原地重试恢复入口', async source => {
    let recover = false
    const queries: string[] = []
    server.use(http.get('/api/v1/projects', ({ request }) => {
      queries.push(new URL(request.url).searchParams.get('permission') ?? '')
      return recover
        ? HttpResponse.json([{ id: 1, name: 'web-app', scm_type: 'git', scm_url: '', default_branch: 'main', created_at: 1, updated_at: 1, pipeline_count: 5 }])
        : HttpResponse.json({ code: 'INTERNAL', message: '暂不可用' }, { status: 500 })
    }))
    await mountAt(source, false)
    await vi.waitFor(() => expect(wrapper.find('[data-testid="pipeline-create-permission-error"]').exists()).toBe(true))
    expect(wrapper.find('[data-testid="pipeline-create-unavailable"]').exists()).toBe(false)
    const selector = source === '/pipelines' ? '[data-testid="topbar-cta"]' : '[data-testid="new-pipeline-btn"]'
    expect(wrapper.find(selector).exists()).toBe(false)
    recover = true
    await wrapper.get('[data-testid="pipeline-create-permission-retry"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find(selector).exists()).toBe(true))
    expect(queries).toEqual(['admin', 'admin'])
  })

  it('项目未出现在管理清单时，两个项目内入口都不可见', async () => {
    server.use(
      http.get('/api/v1/projects', () => HttpResponse.json([])),
      http.get('/api/v1/pipelines', () => HttpResponse.json({ items: [], total: 0 })),
      http.get('/api/v1/projects/web-app/members', () => HttpResponse.json({ code: 'FORBIDDEN' }, { status: 403 })),
    )
    await mountAt('/projects/web-app', false)
    await vi.waitFor(() => expect(wrapper.find('[data-testid="pipelines-empty"]').exists()).toBe(true))
    expect(wrapper.find('[data-testid="new-pipeline-btn"]').exists()).toBe(false)
    expect(wrapper.find('[data-testid="pipeline-empty-create-btn"]').exists()).toBe(false)
  })

  it('412 后可原地改名，保留编排内容并用条件请求创建新名称', async () => {
    await mountAt('/pipelines')
    await enterDraft('[data-testid="topbar-cta"]', 'flow-conflict')
    await wrapper.get('[name="track-add-stage"]').trigger('click')
    // 模拟预检后别人抢先创建；真实 mock 契约裁决条件首建为 412。
    await pipelinesApi.createDefinition('web-app', 'flow-conflict', { name: 'flow-conflict', parameters: [], env: [], stages: [] })
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.text()).toContain('草稿已保留'))
    await wrapper.get('input[name="editor-draft-name"]').setValue('flow-renamed')
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.get('.editor-rev-value').text()).toBe('1'))
    const created = await pipelinesApi.getDefinition('web-app', 'flow-renamed')
    expect(created.definition.stages).toEqual([{ name: 'stage-1', jobs: [] }])
    expect(router.currentRoute.value.params.pipeline).toBe('flow-renamed')
    expect(writes.at(-1)!.headers.get('If-None-Match')).toBe('*')
    expect((await pipelinesApi.getDefinition('web-app', 'flow-conflict')).revision).toBe(1)
  })

  it.each([403, 404, 422, 0])('首次保存失败 %i 保留脏草稿，可继续编辑和条件重试', async status => {
    const name = `flow-failure-${status}`
    await mountAt('/pipelines')
    await enterDraft('[data-testid="topbar-cta"]', name)
    await wrapper.get('[name="track-add-stage"]').trigger('click')
    server.use(http.put(`/api/v1/projects/web-app/pipelines/${name}`, () => status === 0
      ? HttpResponse.error()
      : HttpResponse.json({ code: status === 422 ? 'VALIDATION_FAILED' : 'ERROR', message: 'failed', detail: { errors: [{ path: 'name', message: '名称校验失败' }] } }, { status })))
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('[role="alert"]').exists()).toBe(true))
    expect(wrapper.get('[data-testid="editor-new-badge"]').text()).toContain('未保存')
    expect(wrapper.findAll('.stage-column')).toHaveLength(1)
    expect((wrapper.get('input[name="editor-draft-name"]').element as HTMLInputElement).value).toBe(name)
    const unload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(unload)
    expect(unload.defaultPrevented).toBe(true)
    await router.push('/projects/web-app/pipelines/main')
    await vi.waitFor(() => expect(document.querySelector('[data-testid="unsaved-continue"]')).toBeTruthy())
    ;(document.querySelector('[data-testid="unsaved-continue"]') as HTMLElement).click()
    expect(router.currentRoute.value.params.pipeline).toBe(name)
    server.resetHandlers()
    await wrapper.get('[name="editor-save"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.get('.editor-rev-value').text()).toBe('1'))
    expect(writes.at(-1)!.headers.get('If-None-Match')).toBe('*')
  })

  it('新建态中英文身份完整；可逆编辑回到基线后无需未保存提示', async () => {
    await mountAt('/pipelines')
    await enterDraft('[data-testid="topbar-cta"]', 'flow-language')
    setLocale('en-US')
    await wrapper.vm.$nextTick()
    expect(wrapper.get('[data-testid="editor-new-badge"]').text()).toContain('Unsaved')
    expect(wrapper.get('[name="editor-save"]').text()).toBe('Save & create')
    expect(wrapper.get('[data-testid="editor-unsaved-hint"]').text()).toContain('web-app')
    await wrapper.get('[name="track-add-stage"]').trigger('click')
    await wrapper.get('[name="stage-0-delete"]').trigger('click')
    const unload = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(unload)
    expect(unload.defaultPrevented).toBe(false)
    router.back()
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/pipelines'))
  })
})

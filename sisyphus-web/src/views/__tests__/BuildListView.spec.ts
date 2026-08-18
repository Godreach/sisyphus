// 构建列表页行为测试（票 B4-T4，ADR-0006）：只测外部行为——分页渲染、
// 状态过滤、行点击进入构建详情、空/错误态。API 层以 fetch mock 驱动。
//
// 注意：组件在 onMounted 即触发首次请求（onMounted 与 watch 初始各一次），
// 因此各测试须在 mount 前设置好 fetch 实现（mockImplementation 每次调用给
// 新 Response，避免复用同一 Response 读空 body）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

import BuildListView from '@/views/BuildListView.vue'
import { i18n, setLocale } from '@/i18n'

/** 构造 mock JSON 响应。 */
function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

function buildItem(overrides: Record<string, unknown> = {}) {
  return {
    number: 1,
    pipeline_name: 'release',
    status: 'succeeded',
    trigger: 'manual',
    trigger_by: 'alice',
    attempt: 1,
    started_at: 1_700_000_000_000,
    finished_at: 1_700_000_010_000,
    cancelled_at: null,
    ...overrides,
  }
}

function listBody(items: unknown[], total: number) {
  return { items, total, page: 1, limit: 20 }
}

describe('BuildListView（分页 + 状态过滤）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper
  const fetchMock = vi.fn()

  function mountView(): VueWrapper {
    wrapper = mount(BuildListView, {
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
        { path: '/projects/:name/pipelines/:pipeline/builds', name: 'build-list', component: { template: '<div />' } },
        { path: '/projects/:name/pipelines/:pipeline/builds/:number', name: 'build-detail', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/projects/:name/pipelines/:pipeline', name: 'pipeline-edit', component: { template: '<div />' } },
      ],
    })
    await router.push('/projects/demo/pipelines/release/builds')
    await router.isReady()
    globalThis.fetch = fetchMock
    fetchMock.mockClear()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('加载列表：请求带分页参数，行渲染状态徽标与元数据', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse(
          200,
          listBody([buildItem(), buildItem({ number: 2, status: 'running', trigger_by: 'bob' })], 2),
        ),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.build-list-row')).toHaveLength(2))

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe('/api/v1/projects/demo/pipelines/release/builds?page=1&limit=20')
    expect(init.method).toBe('GET')

    expect(wrapper.text()).toContain('#1')
    expect(wrapper.text()).toContain('#2')
    expect(wrapper.text()).toContain('成功')
    expect(wrapper.text()).toContain('运行中')
    expect(wrapper.text()).toContain('alice')
    expect(wrapper.text()).toContain('bob')
  })

  it('状态过滤：选择 failed 后重新请求带 status 参数', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse(200, listBody([buildItem({ status: 'failed' })], 1)),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.build-list-row')).toHaveLength(1))

    await wrapper.get('select[name="status-filter"]').setValue('failed')
    await vi.waitFor(() => {
      const last = fetchMock.mock.calls[fetchMock.mock.calls.length - 1] as [string, RequestInit]
      expect(last[0]).toContain('status=failed')
    })
  })

  it('行点击进入构建详情（路由跳转带 number 参数）', async () => {
    const pushSpy = vi.spyOn(router, 'push')
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(200, listBody([buildItem({ number: 3 })], 1))),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.build-list-row')).toHaveLength(1))
    await wrapper.get('.build-list-row').trigger('click')
    await vi.waitFor(() => expect(pushSpy).toHaveBeenCalled())
    expect(pushSpy).toHaveBeenCalledWith({
      name: 'build-detail',
      params: { name: 'demo', pipeline: 'release', number: '3' },
    })
  })

  it('空列表显示空态文案', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(200, listBody([], 0))),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.text()).toContain('暂无构建'))
  })

  it('加载失败（404）显示错误文案', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse(404, { code: 'NOT_FOUND', message: '项目不存在' }),
      ),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.get('[role="alert"]').text()).toContain('项目不存在'))
  })

  it('分页：下一页请求带 page=2', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(200, listBody([buildItem({ number: 30 })], 30))),
    )
    mountView()
    await vi.waitFor(() => expect(wrapper.findAll('.build-list-row')).toHaveLength(1))

    await wrapper.get('.build-list-pagination .btn:last-child').trigger('click')
    await vi.waitFor(() => {
      const last = fetchMock.mock.calls[fetchMock.mock.calls.length - 1] as [string, RequestInit]
      expect(last[0]).toContain('page=2')
    })
  })
})

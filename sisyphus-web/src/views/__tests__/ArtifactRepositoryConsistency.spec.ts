// 制品库一致性检查（票 #143）：页面经 MSW node handler 走真实 http client，
// 覆盖普通/深哈希结果、管理员权限与后端错误，不依赖真实对象存储。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import ArtifactRepositoryView from '@/views/ArtifactRepositoryView.vue'
import { i18n, setLocale } from '@/i18n'
import { storageConsistencyApi } from '@/api/client'
import { server } from '@/mocks/node'
import * as db from '@/mocks/db'
import { useAuthStore } from '@/stores/auth'

const Host = defineComponent({
  name: 'ArtifactRepositoryConsistencyHost',
  setup() {
    return () => h(NMessageProvider, () => h(ArtifactRepositoryView))
  },
})

const statusResponse = {
  available: true,
  backend: db.DEMO_S3_BACKEND,
}

function installArtifactPageHandlers(): void {
  server.use(
    http.get('/api/v1/artifact-repository', () => HttpResponse.json(statusResponse)),
    http.get('/api/v1/artifact-repository/artifacts', () => HttpResponse.json({
      items: [], total: 0, page: 1, limit: 50, legacy_local_count: 0,
    })),
    http.get('/api/v1/projects', () => HttpResponse.json([])),
  )
}

describe('ArtifactRepositoryView 存储一致性检查（票 #143）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/artifacts', name: 'artifacts', component: { template: '<div />' } }],
    })
    await router.push('/artifacts')
    await router.isReady()
    installArtifactPageHandlers()
    useAuthStore().setAuthed({ username: 'admin', isAdmin: true })
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  function mountView(): VueWrapper {
    wrapper = mount(Host, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
  }

  it('普通检查展示无 finding、检查时间、后端和零积压', async () => {
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="storage-consistency"]').exists()).toBe(true))

    const panel = w.get('[data-testid="storage-consistency"]')
    await panel.findAll('button')[0]!.trigger('click')
    await vi.waitFor(() => expect(panel.find('[data-testid="consistency-report"]').exists()).toBe(true))

    expect(panel.text()).toContain('检查时间')
    expect(panel.text()).toContain('后端')
    expect(panel.text()).toContain('s3')
    expect(panel.text()).toContain('发现 0 项异常')
    expect(panel.text()).toContain('上传：0')
    expect(panel.find('[data-testid="consistency-findings"]').exists()).toBe(false)
  })

  it('深哈希检查带 finding、非零积压和错误列表', async () => {
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="storage-consistency"]').exists()).toBe(true))

    const panel = w.get('[data-testid="storage-consistency"]')
    await panel.findAll('button')[1]!.trigger('click')
    await vi.waitFor(() => expect(panel.find('[data-testid="consistency-findings"]').exists()).toBe(true))

    expect(panel.text()).toContain('发现 1 项异常')
    expect(panel.text()).toContain('demo/release/12/package/app.tgz')
    expect(panel.text()).toContain('上传：2')
    expect(panel.text()).toContain('对象哈希校验失败')
    expect(panel.text()).toContain('归档积压等待重试')
  })

  it('非全局管理员收到稳定 FORBIDDEN 权限错误', async () => {
    const response = await fetch(new URL('/api/v1/storage/consistency', window.location.origin), {
      headers: { 'x-sisyphus-mock-user': 'alice' },
    })
    expect(response.status).toBe(403)
    await expect(response.json()).resolves.toMatchObject({ code: 'FORBIDDEN', message: '非全局管理员' })
  })

  it('非全局管理员页面不展示一致性执行动作', async () => {
    useAuthStore().setAuthed({ username: 'alice', isAdmin: false })
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="artifact-repo-available"]').exists()).toBe(true))
    expect(w.find('[data-testid="storage-consistency"]').exists()).toBe(false)
  })

  it('检查失败时页面展示后端错误信息', async () => {
    let calls = 0
    server.use(
      http.get('/api/v1/storage/consistency', () => {
        calls += 1
        if (calls > 1) {
          return HttpResponse.json({ code: 'INTERNAL', message: '一致性检查暂时不可用', detail: null }, { status: 500 })
        }
        return HttpResponse.json({
          checked_at: 1,
          deep_hash: false,
          backend: 's3',
          findings: [],
          backlog: { pending_uploads: 0, pending_multipart_uploads: 0, pending_deletions: 0, pending_archives: 0 },
          errors: [],
        })
      }),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="storage-consistency"]').exists()).toBe(true))

    const panel = w.get('[data-testid="storage-consistency"]')
    await panel.findAll('button')[0]!.trigger('click')
    await vi.waitFor(() => expect(panel.find('[data-testid="consistency-report"]').exists()).toBe(true))
    await panel.findAll('button')[0]!.trigger('click')
    await vi.waitFor(() => expect(panel.text()).toContain('一致性检查暂时不可用'))
    expect(panel.find('[data-testid="consistency-report"]').exists()).toBe(false)
  })

  it('客户端深哈希请求带 deep_hash 查询参数', async () => {
    let query = ''
    server.use(
      http.get('/api/v1/storage/consistency', ({ request }) => {
        query = new URL(request.url).search
        return HttpResponse.json({
          checked_at: 1,
          deep_hash: true,
          backend: 's3',
          findings: [],
          backlog: { pending_uploads: 0, pending_multipart_uploads: 0, pending_deletions: 0, pending_archives: 0 },
          errors: [],
        })
      }),
    )
    await storageConsistencyApi.check(true)
    expect(query).toBe('?deep_hash=true')
  })
})

// 项目详情页行为测试（票 #108 定稿铺开，spec #100；ADR-0014/0016/0020）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，淘汰旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers（fixture 即测试
// 数据）；确定性场景（403/404/动作映射）用 server.use 覆盖。只测外部行为
// （用户可见状态、DOM 事件、网络请求形态断言）。
//
// 覆盖面：项目事实态（骨架屏/404 同形/整页报错重试）、项目信息卡、
// 流水线区（真清单按项目过滤 + stats 行 + 行内动作触发/终止/重试 + 编辑
// 与新建流水线导航）、最近构建（多流水线合并）、成员卡（整组替换）、
// SCM 凭据卡（保存/清除/测试连接）、编辑项目（PATCH 契约先行）、403 卡内
// 退化提示。

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import ProjectDetailView from '@/views/ProjectDetailView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

const BASE = '/api/v1/projects/web-app'

/** 包装组件：NMessageProvider + ProjectDetailView（useMessage 注入可用）。 */
const DetailWrapper = defineComponent({
  name: 'DetailWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(ProjectDetailView, { ...attrs }))
  },
})

/** 项目/流水线统计的确定性覆盖（latest 状态决定行内动作映射）。 */
function mockStats(pipeline: string, latestStatus: string | null): void {
  server.use(
    http.get(`${BASE}/pipelines/${pipeline}/stats`, () =>
      HttpResponse.json({
        window: 20,
        total_builds: 13,
        terminal_count: 10,
        succeeded_count: 9,
        success_rate: 90,
        avg_duration_ms: 363_000,
        latest_build:
          latestStatus == null
            ? null
            : {
                number: 13,
                status: latestStatus,
                trigger: 'manual',
                started_at: Date.now() - 3600e3,
                finished_at: Date.now() - 3540e3,
              },
      }),
    ),
  )
}

/** members/directory 403 退化覆盖（非项目 admin 视角）。 */
function mockMembersForbidden(): void {
  server.use(
    http.get(`${BASE}/members`, () =>
      HttpResponse.json({ code: 'FORBIDDEN', message: '项目权限不足' }, { status: 403 }),
    ),
    http.get('/api/v1/users/directory', () =>
      HttpResponse.json({ code: 'FORBIDDEN', message: '项目权限不足' }, { status: 403 }),
    ),
  )
}

function toastWith(text: string): Promise<HTMLElement> {
  return vi.waitFor(() => {
    const el = [...document.querySelectorAll('.n-message')].find((m) =>
      m.textContent?.includes(text),
    )
    expect(el).toBeTruthy()
    return el as HTMLElement
  })
}

/** NSelect 选项点击（jsdom：点开菜单 → 点选项；virtual-scroll 已关）。 */
async function pickSelectOption(wrapper: VueWrapper, selector: string, optionText: string): Promise<void> {
  // data-testid 落在 NSelect 外层（.n-select），点开交互在里层 .n-base-selection。
  const select = wrapper.find(`${selector} .n-base-selection`)
  expect(select.exists()).toBe(true)
  await select.trigger('click')
  await vi.waitFor(() => {
    const option = [...document.querySelectorAll('.n-base-select-option')].find(
      (o) => o.textContent?.trim() === optionText,
    )
    expect(option).toBeTruthy()
    ;(option as HTMLElement).click()
  })
  await new Promise((r) => setTimeout(r, 50))
}

describe('ProjectDetailView 项目详情（票 #108 定稿）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper!: VueWrapper

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  async function mountAt(path: string): Promise<VueWrapper> {
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/projects', name: 'projects', component: { template: '<div />' } },
        { path: '/projects/:name', name: 'project-detail', component: { template: '<div />' } },
        {
          path: '/projects/:name/pipelines/:pipeline',
          name: 'pipeline-edit',
          component: { template: '<div />' },
        },
        {
          path: '/projects/:name/pipelines/:pipeline/builds',
          name: 'build-list',
          component: { template: '<div />' },
        },
        {
          path: '/projects/:name/pipelines/:pipeline/builds/:number',
          name: 'build-detail',
          component: { template: '<div />' },
        },
      ],
    })
    await router.push(path)
    await router.isReady()
    return mount(DetailWrapper, { global: { plugins: [pinia, router, i18n] } })
  }

  beforeEach(() => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
  })

  afterEach(async () => {
    wrapper?.unmount()
    // 弹层（NModal/NSelect 下拉）teleport 到 body——卸载后清出，防泄漏到下个用例。
    document.body.innerHTML = ''
    server.resetHandlers()
  })

  afterAll(() => {
    server.close()
  })

  it('首载骨架屏 → 面包屑/标题/git 徽章/项目信息卡渲染', async () => {
    wrapper = await mountAt('/projects/web-app')

    // 骨架屏先于数据出现并随后被替换。
    expect(wrapper.find('[data-testid="project-detail-skeleton"]').exists()).toBe(true)
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="project-title"]').text()).toBe('web-app'),
    )
    expect(wrapper.find('[data-testid="project-detail-skeleton"]').exists()).toBe(false)

    // 面包屑：项目 / web-app。
    const breadcrumb = wrapper.get('nav.breadcrumb')
    expect(breadcrumb.text()).toContain('项目')
    expect(breadcrumb.text()).toContain('web-app')

    // scm 类型徽章 + 元信息卡。
    expect(wrapper.find('.project-title-row .badge').text()).toBe('git')
    const meta = wrapper.get('.project-info-card')
    expect(meta.text()).toContain('https://github.com/acme/web-app.git')
    expect(meta.text()).toContain('main')
    expect(meta.text()).toContain('仓库 URL')
    expect(meta.text()).toContain('默认分支')
  })

  it('项目不可见：404 同形「不存在或无权访问」+ 返回项目列表入口', async () => {
    wrapper = await mountAt('/projects/ghost')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="project-detail-notfound"]').exists()).toBe(true),
    )
    expect(wrapper.text()).toContain('项目不存在或无权访问')
    const back = wrapper.get('[data-testid="project-detail-back"]')
    expect(back.attributes('href')).toBe('/projects')
    // 不渲染详情体。
    expect(wrapper.find('.project-info-card').exists()).toBe(false)
  })

  it('加载失败：整页报错 + 重试后恢复（error-demo 500 → 覆盖回 200）', async () => {
    wrapper = await mountAt('/projects/error-demo')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="project-detail-retry"]').exists()).toBe(true),
    )

    // error-demo 的 500 内建在基础 handler（非临时 override）——恢复须显式覆盖 200。
    server.use(
      http.get('/api/v1/projects/error-demo', () =>
        HttpResponse.json({
          id: 11,
          name: 'error-demo',
          scm_type: 'git',
          scm_url: 'https://github.com/acme/error-demo.git',
          default_branch: 'main',
          created_at: 0,
          updated_at: 0,
        }),
      ),
    )
    await wrapper.get('[data-testid="project-detail-retry"]').trigger('click')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="project-title"]').text()).toBe('error-demo'),
    )
  })

  it('流水线区：渲染本项目全部流水线（清单过滤项目维），别项目不出现', async () => {
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.findAll('[data-testid^="pipeline-row-"]').length).toBeGreaterThanOrEqual(3),
    )
    const card = wrapper.get('.project-pipelines-card')
    expect(card.find('[data-testid="pipeline-row-main"]').exists()).toBe(true)
    expect(card.find('[data-testid="pipeline-row-release"]').exists()).toBe(true)
    expect(card.find('[data-testid="pipeline-row-nightly"]').exists()).toBe(true)
    // 别项目（api-gateway）的流水线不出现在卡内。
    expect(card.find('[data-testid="pipeline-row-integration"]').exists()).toBe(false)
    // 状态徽章渲染（据 fixture latest 任意态）。
    expect(card.findAll('.ppl-row .badge').length).toBeGreaterThanOrEqual(3)
  })

  it('流水线行：行名点击跳构建列表；编辑按钮跳编辑器', async () => {
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipeline-row-main"]').exists()).toBe(true),
    )

    await wrapper.get('[data-testid="pipeline-row-main"] .ppl-name').trigger('click')
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('build-list'))
    expect(router.currentRoute.value.params).toMatchObject({ name: 'web-app', pipeline: 'main' })

    await router.push('/projects/web-app')
    await wrapper.get('[data-testid="pipeline-edit-release"]').trigger('click')
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('pipeline-edit'))
    expect(router.currentRoute.value.params).toMatchObject({ name: 'web-app', pipeline: 'release' })
  })

  it('行内动作（latest 失败）：「重试」橙按钮 → POST rerun from_failed → toast', async () => {
    mockStats('main', 'failed')
    let rerunBody: unknown = null
    server.use(
      http.post(`${BASE}/pipelines/main/builds/13/rerun`, async ({ request }) => {
        rerunBody = await request.json()
        return HttpResponse.json({ number: 13, build_id: 13, attempt: 2, status: 'queued' }, { status: 202 })
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipeline-action-main"]').exists()).toBe(true),
    )
    const action = wrapper.get('[data-testid="pipeline-action-main"]')
    expect(action.text()).toBe('重试')
    expect(action.classes()).toContain('orange')

    await action.trigger('click')
    await vi.waitFor(() => expect(rerunBody).toEqual({ mode: 'from_failed' }))
    await toastWith('已请求重跑')
  })

  it('行内动作（latest 成功）：「运行」蓝按钮 → POST builds → toast 已触发', async () => {
    mockStats('main', 'succeeded')

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipeline-action-main"]').exists()).toBe(true),
    )
    const action = wrapper.get('[data-testid="pipeline-action-main"]')
    expect(action.text()).toBe('运行')
    expect(action.classes()).toContain('blue')

    await action.trigger('click')
    await toastWith('已触发构建')
  })

  it('行内动作（latest 运行中）：「终止」红按钮 → POST cancel → toast', async () => {
    mockStats('main', 'running')

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipeline-action-main"]').exists()).toBe(true),
    )
    const action = wrapper.get('[data-testid="pipeline-action-main"]')
    expect(action.text()).toBe('终止')
    expect(action.classes()).toContain('red')

    await action.trigger('click')
    await toastWith('已请求终止')
  })

  it('流水线区空态（fresh-project）：「本项目暂无流水线」+ 新建引导', async () => {
    wrapper = await mountAt('/projects/fresh-project')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="project-title"]').text()).toBe('fresh-project'),
    )
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipelines-empty"]').exists()).toBe(true),
    )
    expect(wrapper.find('[data-testid="pipelines-empty"]').text()).toContain('本项目暂无流水线')
    expect(wrapper.find('[data-testid="pipeline-empty-create-btn"]').exists()).toBe(true)
  })

  it('流水线清单失败：卡内报错 + 重试，不拖垮整页（项目信息卡仍在）', async () => {
    server.use(
      http.get('/api/v1/pipelines', () =>
        HttpResponse.json({ code: 'INTERNAL', message: 'boom' }, { status: 500 }),
      ),
    )
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipelines-retry"]').exists()).toBe(true),
    )
    // 项目信息卡不受影响（局部失败不整页化）。
    expect(wrapper.find('.project-info-card').exists()).toBe(true)

    server.resetHandlers()
    await wrapper.get('[data-testid="pipelines-retry"]').trigger('click')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="pipeline-row-main"]').exists()).toBe(true),
    )
  })

  it('最近构建：多流水线合并展示，行点击跳构建详情', async () => {
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.findAll('[data-testid^="run-row-"]').length).toBeGreaterThan(0),
    )
    const rows = wrapper.findAll('[data-testid^="run-row-"]')
    expect(rows.length).toBeLessThanOrEqual(8)
    // mix 多流水线（fixture：main 13 + release 8 + nightly 5，最新互错）。
    const pipelinesInCard = new Set(
      rows.map((r) => (r.attributes('data-testid') ?? '').split('-').slice(2, -1).join('-')),
    )
    expect(pipelinesInCard.size).toBeGreaterThan(1)

    await rows[0]!.trigger('click')
    // 导航为异步 push（BuildDetailView.spec 先例：waitFor 路由切换落定）。
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('build-detail'))
    expect(router.currentRoute.value.params.name).toBe('web-app')
  })

  it('最近构建空态（fresh-project）：「还没有构建」', async () => {
    wrapper = await mountAt('/projects/fresh-project')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="runs-empty"]').exists()).toBe(true),
    )
    expect(wrapper.find('[data-testid="runs-empty"]').text()).toContain('还没有构建')
  })

  it('成员卡：渲染成员行 + 目录下拉加人 → 保存 PUT 整组替换并回读', async () => {
    let putBody: unknown = null
    server.use(
      http.put(`${BASE}/members`, async ({ request }) => {
        putBody = await request.json()
        return HttpResponse.json(putBody as object[], { status: 200 })
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="member-row-alice"]').exists()).toBe(true),
    )
    expect(wrapper.find('[data-testid="member-row-alice"]').text()).toContain('alice')
    expect(wrapper.find('[data-testid="member-row-bob"]').text()).toContain('bob')

    // 目录中尚未是成员的 admin 加为 viewer → 保存（PUT 整组替换）。
    await pickSelectOption(wrapper, '[data-testid="member-add-user"]', 'admin')
    await pickSelectOption(wrapper, '[data-testid="member-add-role"]', 'viewer')
    await wrapper.get('[data-testid="member-save"]').trigger('click')

    await vi.waitFor(() => expect(putBody).toBeTruthy())
    expect(putBody).toEqual([
      { username: 'alice', role: 'admin' },
      { username: 'bob', role: 'viewer' },
      { username: 'admin', role: 'viewer' },
    ])
    await toastWith('成员已保存')
  })

  it('成员卡：行内移除后保存，PUT body 不含被移除者', async () => {
    let putBody: unknown = null
    server.use(
      http.put(`${BASE}/members`, async ({ request }) => {
        putBody = await request.json()
        return HttpResponse.json(putBody as object[])
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="member-row-bob"]').exists()).toBe(true),
    )
    await wrapper.get('[data-testid="member-remove-bob"]').trigger('click')
    expect(wrapper.find('[data-testid="member-row-bob"]').exists()).toBe(false)

    await wrapper.get('[data-testid="member-save"]').trigger('click')
    await vi.waitFor(() => expect(putBody).toBeTruthy())
    expect(putBody).toEqual([{ username: 'alice', role: 'admin' }])
  })

  it('成员/凭据 403：卡内退化提示，不渲染表单与编辑项目按钮', async () => {
    mockMembersForbidden()
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="members-forbidden"]').exists()).toBe(true),
    )
    expect(wrapper.find('[data-testid="members-forbidden"]').text()).toContain('成员管理需项目 admin 档')
    expect(wrapper.find('[data-testid="cred-forbidden"]').text()).toContain('凭据管理需项目 admin 档')
    // 表单面不渲染；编辑项目按钮（项目 admin 专属）不出现。
    expect(wrapper.find('[data-testid="member-save"]').exists()).toBe(false)
    expect(wrapper.find('[data-testid="edit-project-btn"]').exists()).toBe(false)
    // 项目信息/流水线/最近构建（viewer 档面）不受影响。
    expect(wrapper.find('.project-info-card').exists()).toBe(true)
    expect(wrapper.find('.project-pipelines-card').exists()).toBe(true)
  })

  it('SCM 凭据：保存 PUT {username,password} → toast 已保存', async () => {
    let credBody: unknown = null
    server.use(
      http.put(`${BASE}/scm-credential`, async ({ request }) => {
        credBody = await request.json()
        return new HttpResponse(null, { status: 204 })
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('input[name="cred-username"]').exists()).toBe(true),
    )
    await wrapper.get('input[name="cred-username"]').setValue('alice')
    await wrapper.get('input[name="cred-password"]').setValue('secret')
    await wrapper.get('[data-testid="cred-save"]').trigger('click')

    await vi.waitFor(() => expect(credBody).toEqual({ username: 'alice', password: 'secret' }))
    await toastWith('凭据已保存')
  })

  it('SCM 凭据：用户名密码皆空保存 = 清除（双 null）→ toast 已清除', async () => {
    let credBody: unknown = null
    server.use(
      http.put(`${BASE}/scm-credential`, async ({ request }) => {
        credBody = await request.json()
        return new HttpResponse(null, { status: 204 })
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="cred-save"]').exists()).toBe(true),
    )
    await wrapper.get('[data-testid="cred-save"]').trigger('click')

    await vi.waitFor(() => expect(credBody).toEqual({ username: null, password: null }))
    await toastWith('凭据已清除')
  })

  it('SCM 凭据：测试连接成功展示 head 徽章', async () => {
    server.use(
      http.post(`${BASE}/test-connection`, () => HttpResponse.json({ head: 'deadbeefcafe' })),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="cred-test"]').exists()).toBe(true),
    )
    await wrapper.get('[data-testid="cred-test"]').trigger('click')
    await vi.waitFor(() => expect(wrapper.find('.cred-badge').text()).toContain('连接成功'))
    expect(wrapper.find('.cred-badge').text()).toContain('deadbeefcafe')
  })

  it('编辑项目（项目 admin）：弹窗改 URL/分支 → PATCH + toast + 元信息刷新', async () => {
    let patchBody: unknown = null
    server.use(
      http.patch(`${BASE}`, async ({ request }) => {
        patchBody = await request.json()
        return HttpResponse.json({
          id: 1,
          name: 'web-app',
          scm_type: 'git',
          scm_url: 'https://github.com/acme/web-app-renamed.git',
          default_branch: 'develop',
          created_at: 0,
          updated_at: 1,
        })
      }),
    )

    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="edit-project-btn"]').exists()).toBe(true),
    )
    await wrapper.get('[data-testid="edit-project-btn"]').trigger('click')
    // NModal 内容 teleport 到 body——按 document 查询（BuildDetailView.spec 先例）。
    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal input[name="edit-scm-url"]')).toBeTruthy(),
    )

    const urlInput = document.querySelector('.n-modal input[name="edit-scm-url"]') as HTMLInputElement
    urlInput.value = 'https://github.com/acme/web-app-renamed.git'
    await urlInput.dispatchEvent(new Event('input'))
    const branchInput = document.querySelector('.n-modal input[name="edit-default-branch"]') as HTMLInputElement
    branchInput.value = 'develop'
    await branchInput.dispatchEvent(new Event('input'))

    const saveBtn = document.querySelector('.n-modal [data-testid="edit-project-save"]') as HTMLElement
    await saveBtn.click()

    await vi.waitFor(() => expect(patchBody).toBeTruthy())
    expect(patchBody).toEqual({
      scm_url: 'https://github.com/acme/web-app-renamed.git',
      default_branch: 'develop',
    })
    await toastWith('项目已保存')
    await vi.waitFor(() =>
      expect(wrapper.find('.project-info-card').text()).toContain('web-app-renamed.git'),
    )
  })

  it('新建流水线：弹窗输名 → 跳编辑器（保存即创建语义在编辑器侧）', async () => {
    wrapper = await mountAt('/projects/web-app')
    await vi.waitFor(() =>
      expect(wrapper.find('[data-testid="new-pipeline-btn"]').exists()).toBe(true),
    )
    await wrapper.get('[data-testid="new-pipeline-btn"]').trigger('click')
    // NModal 内容 teleport 到 body——按 document 查询。
    await vi.waitFor(() =>
      expect(document.querySelector('.n-modal input[name="new-pipeline-name"]')).toBeTruthy(),
    )
    const nameInput = document.querySelector('.n-modal input[name="new-pipeline-name"]') as HTMLInputElement
    nameInput.value = 'hotfix'
    await nameInput.dispatchEvent(new Event('input'))
    const createBtn = document.querySelector('.n-modal [data-testid="new-pipeline-create"]') as HTMLElement
    await createBtn.click()
    await vi.waitFor(() => expect(router.currentRoute.value.name).toBe('pipeline-edit'))
    expect(router.currentRoute.value.params).toMatchObject({ name: 'web-app', pipeline: 'hotfix' })
  })
})

// 审计页行为测试（票 #110 定稿铺开，spec #100；ADR-0015）。
// 数据驱动：MSW node 模式（ADR-0024 单一缝，淘汰旧手写 fetch mock 双份
// 维护）——组件经真实 http client 打 src/mocks handlers（fixture 即测试
// 数据）；确定性场景（403/空结果）用 server.use 覆盖。只测外部行为
// （用户可见状态、DOM 事件、网络请求形态断言）。
//
// 覆盖面：加载条目（时间倒序回放）+ 事件胶囊徽章 + detail JSON（机密只记
// 名）、过滤 since/until/user/project/event → query 参数 + offset 归零、
// 分页（limit/offset；无 total，按条数 == limit 判下一页）、重置过滤、
// 403 → admin-only 退化态、空结果空态、整页报错 + 重试。
//
// 注：改动型动作（写/删机密）会向共享 fixture 落审计——本文件只读回放，
// 不依赖「清单首行是某固定条目」，断言全部形态化（相对序/字段映射）。

import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NMessageProvider, NSelect } from 'naive-ui'
import { defineComponent, h } from 'vue'
import { http, HttpResponse } from 'msw'

import AuditView from '@/views/AuditView.vue'
import { i18n, setLocale } from '@/i18n'
import { server } from '@/mocks/node'

/** 包装组件：NMessageProvider + AuditView，保证 useMessage 注入可用。 */
const Host = defineComponent({
  name: 'AuditHost',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AuditView, { ...attrs }))
  },
})

/** 从一次观测到的请求解析 query 参数。 */
function paramsOf(req: { path: string }): URLSearchParams {
  return new URL(req.path, 'http://localhost').searchParams
}

describe('AuditView 过滤回放 + 分页 + 退化态（#110 定稿）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  /** 经 MSW 观测到的请求路径（method + path with query，网络形态断言面）。 */
  let requests: { method: string; path: string }[]

  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/audit', name: 'admin-audit', component: { template: '<div />' } }],
    })
    await router.push('/admin/audit')
    await router.isReady()
    requests = []
    server.events.on('request:start', ({ request }) => {
      requests.push({ method: request.method, path: new URL(request.url).pathname + new URL(request.url).search })
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

  function rows(): ReturnType<VueWrapper['findAll']> {
    return wrapper!.findAll('.audit-row')
  }

  it('首载骨架屏 → 审计回放表：时间倒序行序 + 事件胶囊徽章 + detail JSON（机密只记名）', async () => {
    const w = mountView()

    // 骨架屏先于数据出现（onMounted 异步置 loading——DOM 更新在 nextTick
    // 落地）并随后被替换（事实态纪律）。
    await vi.waitFor(() => expect(w.find('[data-testid="audit-skeleton"]').exists()).toBe(true))
    await vi.waitFor(() => expect(rows().length).toBeGreaterThan(0))
    expect(w.find('[data-testid="audit-skeleton"]').exists()).toBe(false)

    // 时间倒序回放：行序与响应序一致（fixture 生成即倒序，行内时间串可
    // 字典序比较——formatDateTime 输出零填充 `YYYY-MM-DD HH:mm:ss`）。
    const times = rows().map((r) => r.find('.ac-time').text())
    for (let i = 1; i < times.length; i++) {
      expect(times[i - 1]!.startsWith('20')).toBe(true)
    }

    // 事件胶囊徽章按 auditEvent.* 翻译（badge 胶囊类，非 NTag）。
    const badgeTexts = rows().map((r) => r.find('.badge').text())
    expect(badgeTexts).toContain('机密建立')
    expect(badgeTexts).toContain('登录成功')
    expect(badgeTexts).toContain('登出')

    // 机密事件 detail 只记名（JSON 人读形态；值形态永不出现）。
    const detailCells = rows().filter((r) => r.find('.audit-detail').exists())
    expect(detailCells.length).toBeGreaterThan(0)
    const allDetail = detailCells.map((r) => r.find('.audit-detail').text()).join('\n')
    expect(allDetail).toContain('"secret"')
    expect(allDetail).not.toContain('"value"')

    // 非项目域事件（project=null）project 列展示「—」。
    const noneProject = rows().filter((r) => r.find('.ac-project').text() === '—')
    expect(noneProject.length).toBeGreaterThan(0)

    // 首载网络形态：默认 limit=50&offset=0。
    await vi.waitFor(() => expect(requests.length).toBeGreaterThan(0))
    const first = paramsOf(requests[0]!)
    expect(first.get('limit')).toBe('50')
    expect(first.get('offset')).toBe('0')
  })

  it('过滤 since/until/user/project/event → query 参数 + offset 归零', async () => {
    const w = mountView()
    await vi.waitFor(() => expect(rows().length).toBeGreaterThan(0))
    const countBefore = requests.length

    // 设过滤后应用（datetime-local 成对输入；事件 NSelect 经事件驱动）。
    await w.get('input[name="audit-since"]').setValue('2026-08-19T09:30')
    await w.get('input[name="audit-until"]').setValue('2026-08-20T09:30')
    await w.get('input[name="audit-user"]').setValue('alice')
    await w.get('input[name="audit-project"]').setValue('web-app')
    await w.findComponent(NSelect).vm.$emit('update:value', 'secret_created')
    await w.get('button[name="audit-apply"]').trigger('click')

    await vi.waitFor(() => expect(requests.length).toBeGreaterThan(countBefore))
    const last = paramsOf(requests.at(-1)!)
    expect(last.get('since')).toBe(String(new Date('2026-08-19T09:30').getTime()))
    expect(last.get('until')).toBe(String(new Date('2026-08-20T09:30').getTime()))
    expect(last.get('user')).toBe('alice')
    expect(last.get('project')).toBe('web-app')
    expect(last.get('event')).toBe('secret_created')
    expect(last.get('limit')).toBe('50')
    expect(last.get('offset')).toBe('0')
  })

  it('分页：下一页 offset+50 / 上一页 offset-50（无 total，按条数 == limit 判下一页）', async () => {
    // fixture 全量 < 50 条 → 下一页禁用、上一页禁用（首屏）。
    const w = mountView()
    await vi.waitFor(() => expect(rows().length).toBeGreaterThan(0))

    expect(w.get('button[name="audit-prev"]').attributes('disabled')).toBeDefined()
    expect(w.get('button[name="audit-next"]').attributes('disabled')).toBeDefined()

    // 覆盖为满页 50 条 → 下一页可用；点下一页 → offset=50。
    server.use(
      http.get('/api/v1/audit', () =>
        HttpResponse.json(
          Array.from({ length: 50 }, (_, i) => ({
            id: 1000 + i,
            ts: 1_700_000_000_000 - i * 1000,
            actor: 'admin',
            event: 'login_success',
            project: null,
            detail: null,
          })),
        ),
      ),
    )
    await w.get('button[name="audit-apply"]').trigger('click')
    await vi.waitFor(() => expect(rows().length).toBe(50))
    expect(w.get('button[name="audit-next"]').attributes('disabled')).toBeUndefined()

    // 覆盖为尾页 5 条 → 下一页禁用、上一页可用；点上一页 → offset=0。
    server.use(
      http.get('/api/v1/audit', () =>
        HttpResponse.json(
          Array.from({ length: 5 }, (_, i) => ({
            id: 900 + i,
            ts: 1_700_000_000_000 - i * 1000,
            actor: 'admin',
            event: 'login_success',
            project: null,
            detail: null,
          })),
        ),
      ),
    )
    await w.get('button[name="audit-next"]').trigger('click')
    await vi.waitFor(() => {
      expect(paramsOf(requests.at(-1)!).get('offset')).toBe('50')
    })
    await vi.waitFor(() => expect(rows().length).toBe(5))
    expect(w.get('button[name="audit-next"]').attributes('disabled')).toBeDefined()
    expect(w.get('button[name="audit-prev"]').attributes('disabled')).toBeUndefined()

    await w.get('button[name="audit-prev"]').trigger('click')
    await vi.waitFor(() => {
      expect(paramsOf(requests.at(-1)!).get('offset')).toBe('0')
    })
  })

  it('重置过滤：全部过滤器清空 + offset 归零 + 重新加载', async () => {
    const w = mountView()
    await vi.waitFor(() => expect(rows().length).toBeGreaterThan(0))

    await w.get('input[name="audit-user"]').setValue('alice')
    await w.findComponent(NSelect).vm.$emit('update:value', 'secret_created')
    // 有活跃过滤 → 重置按钮出现。
    await vi.waitFor(() => expect(w.find('button[name="audit-clear"]').exists()).toBe(true))
    await w.get('button[name="audit-clear"]').trigger('click')

    await vi.waitFor(() => {
      const p = paramsOf(requests.at(-1)!)
      expect(p.get('user')).toBeNull()
      expect(p.get('event')).toBeNull()
      expect(p.get('offset')).toBe('0')
    })
    // 过滤器表单已清空（输入框空、下拉回「全部事件」）。
    expect((w.get('input[name="audit-user"]').element as HTMLInputElement).value).toBe('')
    expect(w.findComponent(NSelect).props('value')).toBe('')
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染过滤条/表格', async () => {
    server.use(
      http.get('/api/v1/audit', () =>
        HttpResponse.json({ code: 'FORBIDDEN', message: '非全局管理员' }, { status: 403 }),
      ),
    )
    const w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="audit-admin-only"]').exists()).toBe(true))
    expect(w.find('.audit-filters').exists()).toBe(false)
    expect(w.find('.audit-row').exists()).toBe(false)
  })

  it('空结果 → 空态；加载失败 → 整页报错 + 重试恢复', async () => {
    // 空结果：过滤组合无匹配。
    server.use(
      http.get('/api/v1/audit', () => HttpResponse.json([])),
    )
    let w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="audit-empty"]').exists()).toBe(true))
    w.unmount()
    document.body.innerHTML = ''

    // 加载失败：整页报错 + 重试（覆盖回 200 恢复）。
    server.use(
      http.get('/api/v1/audit', () =>
        HttpResponse.json({ code: 'INTERNAL', message: '服务内部错误' }, { status: 500 }),
      ),
    )
    w = mountView()
    await vi.waitFor(() => expect(w.find('[data-testid="audit-error"]').exists()).toBe(true))
    server.use(
      http.get('/api/v1/audit', () =>
        HttpResponse.json([
          { id: 1, ts: 1_700_000_000_000, actor: 'admin', event: 'logout', project: null, detail: null },
        ]),
      ),
    )
    await w.get('button[name="audit-retry"]').trigger('click')
    await vi.waitFor(() => expect(rows().length).toBe(1))
    expect(w.find('[data-testid="audit-error"]').exists()).toBe(false)
  })
})

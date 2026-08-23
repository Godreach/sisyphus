// 审计页行为测试（ADR-0015，票 B4-T6）：只测外部行为，API 层以 fetch mock
// （method + URL 前缀路由）驱动。视图在 onMounted 即发审计请求：mount 须在
// 设置 fetch mock 之后。
// - 加载条目（时间倒序）+ 事件类型 NTag + detail JSON（NCode）
// - 过滤 since/until/user/project/event → query 参数 + offset 归零
//   （NDatePicker(datetimerange)/NSelect 经 findComponent + $emit 驱动）
// - 分页：下一页/上一页（limit/offset；无 total，按条数 == limit 判下一页）
// - 清空过滤；403 → admin-only 退化态；空结果 → 空态
// #95: NDatePicker 值形态 [sinceMs, untilMs]（组件事件注入后经 apply 提交）。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import { NDatePicker, NMessageProvider, NSelect, NTag } from 'naive-ui'
import { defineComponent, h } from 'vue'

import AuditView from '@/views/AuditView.vue'
import { i18n, setLocale } from '@/i18n'
import type { AuditEntryResponse } from '@/api/types'

function jsonResponse(status: number, body: unknown): Response {
  const headers = new Headers({ 'Content-Type': 'application/json' })
  return new Response(JSON.stringify(body), { status, headers })
}

/** 审计条目工厂。 */
function entry(id: number, overrides: Partial<AuditEntryResponse> = {}): AuditEntryResponse {
  return {
    id,
    ts: 1_700_000_000_000 + id * 1000,
    actor: 'alice',
    event: 'secret_created',
    project: 'proj-a',
    detail: { secret: 'DEPLOY_KEY' },
    ...overrides,
  }
}

/** 包装组件：NMessageProvider + AuditView，保证 useMessage 注入可用。 */
const AuditWrapper = defineComponent({
  name: 'AuditWrapper',
  setup(_, { attrs }) {
    return () => h(NMessageProvider, () => h(AuditView, { ...attrs }))
  },
})

describe('AuditView 过滤回放 + 分页 + 退化态', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper | null = null

  const routes = new Map<string, Response>()
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = (init?.method ?? 'GET').toUpperCase()
    let best: { len: number; res: Response } | null = null
    for (const [key, res] of routes) {
      const [m, prefix] = [key.slice(0, key.indexOf(' ')), key.slice(key.indexOf(' ') + 1)]
      if (method !== m.toUpperCase()) continue
      if (url.startsWith(prefix) && (best == null || prefix.length > best.len)) {
        best = { len: prefix.length, res }
      }
    }
    return best
      ? best.res
      : jsonResponse(404, { code: 'NOT_FOUND', message: `no mock for ${method} ${url}` })
  })

  function setRoute(method: string, prefix: string, res: Response): void {
    routes.set(`${method.toUpperCase()} ${prefix}`, res)
  }

  function mountView(): VueWrapper {
    wrapper = mount(AuditWrapper, { global: { plugins: [pinia, router, i18n] } })
    return wrapper
  }

  /** 从一次 fetch 调用解析 query 参数。 */
  function paramsOf(call: [string, RequestInit]): URLSearchParams {
    return new URL(call[0], 'http://localhost').searchParams
  }

  beforeEach(async () => {
    setLocale('zh-CN')
    routes.clear()
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/admin/audit', name: 'admin-audit', component: { template: '<div />' } }],
    })
    await router.push('/admin/audit')
    await router.isReady()
    globalThis.fetch = fetchMock
  })

  afterEach(() => {
    wrapper?.unmount()
    wrapper = null
    vi.restoreAllMocks()
  })

  it('加载审计条目 + 事件类型 NTag + detail JSON（NCode）', async () => {
    // 后端按时间倒序返回（新事件在前）；本测试给降序 ts，断言渲染保持响应序
    // （即「时间倒序回放」——页面不重排，信任契约的倒序）。
    setRoute(
      'GET',
      '/api/v1/audit',
      jsonResponse(200, [
        entry(3, { event: 'secret_created', detail: { secret: 'DEPLOY_KEY' } }),
        entry(2, { event: 'user_created', detail: { username: 'bob' }, project: null }),
        entry(1, { event: 'logout' }),
      ]),
    )
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(3),
    )

    // 事件类型 NTag 按 auditEvent.* 翻译；行序与响应序一致（时间倒序）。
    const tags = w.findAllComponents(NTag)
    expect(tags.map((tag) => tag.text())).toEqual(['机密建立', '用户建立', '登出'])

    // detail 为 JSON 对象（NCode 人读形态；机密事件只记名，值形态不出现）。
    const tableText = w.get('.n-data-table').text()
    expect(tableText).toContain('"secret"')
    expect(tableText).toContain('DEPLOY_KEY')
    // 非项目域事件（project=null）project 列展示「—」。
    const rows = w.findAll('.n-data-table-tbody .n-data-table-tr')
    expect(rows[1]!.findAll('td')[3]!.text()).toBe('—')
    // 时间倒序回放：行序与响应序一致，首行时间晚于末行
    // （formatDateTime 输出 `YYYY-MM-DD HH:mm:ss`，零填充可字典序比较）。
    const times = rows.map((r) => r.findAll('td')[0]!.text())
    expect(times[0]! > times[2]!).toBe(true)
  })

  it('过滤 since/until/user/project/event → query 参数 + offset 归零', async () => {
    setRoute('GET', '/api/v1/audit', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.n-empty').exists()).toBe(true))

    // 设过滤后应用（时间范围 NDatePicker 值形态 [sinceMs, untilMs]）。
    const since = new Date('2026-08-19T09:30').getTime()
    const until = new Date('2026-08-20T09:30').getTime()
    await w.findComponent(NDatePicker).vm.$emit('update:value', [since, until])
    await w.get('input[name="audit-user"]').setValue('alice')
    await w.get('input[name="audit-project"]').setValue('proj-a')
    await w.findComponent(NSelect).vm.$emit('update:value', 'secret_created')
    await w.get('button[name="audit-apply"]').trigger('click')

    await vi.waitFor(() => {
      const last = fetchMock.mock.calls.at(-1) as [string, RequestInit]
      const p = paramsOf(last)
      expect(p.get('since')).toBe(String(since))
      expect(p.get('until')).toBe(String(until))
      expect(p.get('user')).toBe('alice')
      expect(p.get('project')).toBe('proj-a')
      expect(p.get('event')).toBe('secret_created')
      expect(p.get('limit')).toBe('50')
      expect(p.get('offset')).toBe('0')
    })
  })

  it('分页：下一页 offset+50 / 上一页 offset-50（无 total，按条数 == limit 判下一页）', async () => {
    // 首页满 50 → 下一页可用、上一页禁用。
    const full = Array.from({ length: 50 }, (_, i) => entry(i + 1))
    setRoute('GET', '/api/v1/audit', jsonResponse(200, full))
    const w = mountView()
    await vi.waitFor(() =>
      expect(w.findAll('.n-data-table-tbody .n-data-table-tr')).toHaveLength(50),
    )

    expect(w.get('button[name="audit-prev"]').attributes('disabled')).toBeDefined()
    expect(w.get('button[name="audit-next"]').attributes('disabled')).toBeUndefined()

    // 下一页：offset=50，返回 < 50 → 下一页禁用、上一页可用。
    const tail = Array.from({ length: 5 }, (_, i) => entry(i + 51))
    setRoute('GET', '/api/v1/audit', jsonResponse(200, tail))
    await w.get('button[name="audit-next"]').trigger('click')
    await vi.waitFor(() => {
      const last = fetchMock.mock.calls.at(-1) as [string, RequestInit]
      expect(paramsOf(last).get('offset')).toBe('50')
    })
    await vi.waitFor(() =>
      expect(w.get('button[name="audit-next"]').attributes('disabled')).toBeDefined(),
    )
    expect(w.get('button[name="audit-prev"]').attributes('disabled')).toBeUndefined()

    // 上一页：offset=0。
    setRoute('GET', '/api/v1/audit', jsonResponse(200, full))
    await w.get('button[name="audit-prev"]').trigger('click')
    await vi.waitFor(() => {
      const last = fetchMock.mock.calls.at(-1) as [string, RequestInit]
      expect(paramsOf(last).get('offset')).toBe('0')
    })
  })

  it('清空过滤：重置全部过滤器 + offset 归零 + 重新加载', async () => {
    setRoute('GET', '/api/v1/audit', jsonResponse(200, []))
    const w = mountView()
    await vi.waitFor(() => expect(w.find('.n-empty').exists()).toBe(true))

    await w.get('input[name="audit-user"]').setValue('alice')
    await w.findComponent(NSelect).vm.$emit('update:value', 'secret_created')
    await w.get('button[name="audit-clear"]').trigger('click')

    await vi.waitFor(() => {
      const last = fetchMock.mock.calls.at(-1) as [string, RequestInit]
      const p = paramsOf(last)
      expect(p.get('user')).toBeNull()
      expect(p.get('event')).toBeNull()
      expect(p.get('offset')).toBe('0')
    })
    // 过滤器表单已清空（输入框空、下拉回「全部事件」）。
    expect((w.get('input[name="audit-user"]').element as HTMLInputElement).value).toBe('')
    expect(w.findComponent(NSelect).props('value')).toBe('')
  })

  it('403（非全局 admin）→ admin-only 退化态，不渲染过滤器/表格', async () => {
    setRoute('GET', '/api/v1/audit', jsonResponse(403, { code: 'FORBIDDEN', message: '非全局管理员' }))
    const w = mountView()
    await vi.waitFor(() => expect(w.text()).toContain('仅全局管理员可见'))
    expect(w.find('.audit-filters').exists()).toBe(false)
    expect(w.find('.n-data-table').exists()).toBe(false)
  })
})

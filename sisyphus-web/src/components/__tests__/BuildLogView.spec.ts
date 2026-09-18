import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { mount } from '@vue/test-utils'

import BuildLogView from '@/components/BuildLogView.vue'
import { i18n, setLocale } from '@/i18n'
import { MockEventSource } from '@/mocks/eventSource'
import { server } from '@/mocks/node'
import { FakeEventSource } from '@/test/fakeEventSource'

const props = (buildNumber: number, job: string) => ({
  project: 'web-app',
  pipeline: 'main',
  buildNumber,
  job,
  attempt: 1,
})

describe('BuildLogView 日志归档状态（票 #142）', () => {
  beforeAll(() => {
    server.listen({ onUnhandledRequest: 'error' })
  })

  afterEach(() => {
    server.resetHandlers()
    vi.unstubAllGlobals()
  })

  afterAll(() => {
    server.close()
  })

  it('ready 归档通过 demo SSE 回放历史并以 job_end 收尾', async () => {
    setLocale('zh-CN')
    vi.stubGlobal('EventSource', MockEventSource)
    const wrapper = mount(BuildLogView, {
      props: props(11, 'compile'),
      global: { plugins: [i18n] },
    })
    try {
      await vi.waitFor(() => expect(wrapper.text()).toContain('archive replay: compile succeeded'))
      expect(wrapper.text()).toContain('任务已结束')
      expect(wrapper.text()).not.toContain('待归档')
    } finally {
      wrapper.unmount()
    }
  })

  it('pending 保持实时流并明确显示后台重试状态', async () => {
    setLocale('zh-CN')
    FakeEventSource.install()
    const wrapper = mount(BuildLogView, {
      props: props(10, 'unit-test'),
      global: { plugins: [i18n] },
    })
    try {
      await vi.waitFor(() => expect(wrapper.text()).toContain('待归档'))
      FakeEventSource.latest().dispatchOpen()
      await vi.waitFor(() => expect(wrapper.text()).not.toContain('连接日志流'))
      expect(FakeEventSource.latest().closed).toBe(false)
    } finally {
      wrapper.unmount()
    }
  })

  it('lost 关闭实时流，显示原因和最后可用信息，不冒充空日志成功', async () => {
    setLocale('zh-CN')
    FakeEventSource.install()
    const wrapper = mount(BuildLogView, {
      props: props(9, 'lint'),
      global: { plugins: [i18n] },
    })
    try {
      await vi.waitFor(() => expect(wrapper.text()).toContain('retention_expired'))
      expect(wrapper.text()).toContain('日志已按独立保留期清理')
      expect(wrapper.text()).toContain('17')
      expect(wrapper.text()).not.toContain('暂无日志输出')
      expect(FakeEventSource.latest().closed).toBe(true)
    } finally {
      wrapper.unmount()
    }
  })
})

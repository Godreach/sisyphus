import { afterEach, expect, it, vi } from 'vitest'
import { mount } from '@vue/test-utils'
import BuildLogView from '@/components/BuildLogView.vue'
import { i18n, setLocale } from '@/i18n'
import { FakeEventSource } from '@/test/fakeEventSource'

afterEach(() => vi.unstubAllGlobals())

it('永久丢失显示原因和最后可用信息，不冒充空日志成功', async () => {
  setLocale('zh-CN')
  vi.stubGlobal('EventSource', FakeEventSource)
  vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
    state: 'lost', lost_reason: 'Agent disk permanently destroyed', last_seq: 17,
    size: 1024, execution_finished_at: 1700000000000,
  }), { headers: { 'Content-Type': 'application/json' } })))
  const wrapper = mount(BuildLogView, {
    props: { project: 'demo', pipeline: 'release', buildNumber: 1, job: 'build', attempt: 1 },
    global: { plugins: [i18n] },
  })
  try {
    await vi.waitFor(() => expect(wrapper.text()).toContain('Agent disk permanently destroyed'))
    expect(wrapper.text()).toContain('永久丢失')
    expect(wrapper.text()).toContain('17')
  } finally { wrapper.unmount() }
})

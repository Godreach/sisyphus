// SSE 日志流 mock（票 #101，ADR-0013/0024）：MSW 只拦 fetch/XHR，拦不到
// 浏览器原生 EventSource（自建连接、非 fetch 传输）——mock 模式下以引擎
// 驱动的替身替换全局 EventSource，消费同一套 engine/fixture 数据（单一缝）。
//
// - 与真端点同语义：`from=<seq>` 起播（先补历史再接实时尾随）；命名事件
//   （`event: <type>` + `data: <json>`）；job_end 送达即由客户端关流。
// - 非日志流 URL 的 EventSource 构造模拟首连失败（error → 客户端落
//   degraded 退化态），不吞调用侧的退化标注纪律。

import { logHistory, subscribeLogs } from './engine'

const STREAM_URL_RE =
  /^\/api\/v1\/projects\/([^/]+)\/pipelines\/([^/]+)\/builds\/(\d+)\/jobs\/([^/]+)\/attempts\/(\d+)\/logs\/stream$/

interface MinimalMessageEvent {
  data: string
}

type Listener = (ev: MinimalMessageEvent) => void

export class MockEventSource {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSED = 2

  readyState = MockEventSource.CONNECTING
  private readonly listeners = new Map<string, Set<Listener>>()
  private closed = false
  private unsubscribe: (() => void) | null = null

  constructor(url: string | URL) {
    const parsed = new URL(String(url), window.location.origin)
    const match = STREAM_URL_RE.exec(parsed.pathname)
    if (match == null) {
      // 非日志流：模拟「端点未交付」的首连失败（openLogStream 落 degraded）。
      queueMicrotask(() => this.dispatch('error', ''))
      return
    }

    const [, project, pipeline, number, job, attempt] = match
    const from = Number(parsed.searchParams.get('from') ?? '0')

    queueMicrotask(() => {
      if (this.closed) return
      this.readyState = MockEventSource.OPEN
      this.dispatch('open', '')
      // 先补历史（seq > from，模拟 DB 回放），再订阅实时尾随。
      for (const event of logHistory(project as string, pipeline as string, Number(number), job as string, Number(attempt))) {
        if (this.closed) return
        if (event.seq > from) this.dispatch(event.type, JSON.stringify(event))
      }
      this.unsubscribe = subscribeLogs(
        project as string,
        pipeline as string,
        Number(number),
        job as string,
        Number(attempt),
        (event) => {
          if (this.closed) return
          this.dispatch(event.type, JSON.stringify(event))
        },
      )
    })
  }

  addEventListener(type: string, listener: Listener): void {
    let set = this.listeners.get(type)
    if (set == null) {
      set = new Set()
      this.listeners.set(type, set)
    }
    set.add(listener)
  }

  removeEventListener(type: string, listener: Listener): void {
    this.listeners.get(type)?.delete(listener)
  }

  close(): void {
    this.closed = true
    this.readyState = MockEventSource.CLOSED
    this.unsubscribe?.()
    this.unsubscribe = null
  }

  private dispatch(type: string, data: string): void {
    for (const listener of [...(this.listeners.get(type) ?? [])]) {
      listener({ data })
    }
  }
}

/** mock 模式启用时替换全局 EventSource（dev worker 启动前调用）。 */
export function installMockEventSource(): void {
  ;(window as unknown as { EventSource: unknown }).EventSource = MockEventSource
}

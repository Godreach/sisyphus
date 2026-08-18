// 测试共用替身 EventSource（jsdom 无原生实现，Spec B4 测试缝：mock
// EventSource 替身驱动 SSE 日志流测试——sse.spec 与 BuildDetailView.spec
// 共用同一替身形态）。

/** 替身 EventSource：记录构造 URL、暴露 addEventListener/close，测试可
 *  手动触发命名事件与 open/error。 */
export class FakeEventSource {
  /** 全部实例（按构造序）。 */
  static instances: FakeEventSource[] = []
  readonly url: string
  /** 已注册的监听器（事件名 → 集合）。 */
  listeners = new Map<string, Set<(ev: unknown) => void>>()
  closed = false

  /** 安装替身：把全局 EventSource 替换为收集实例的子类。 */
  static install(): void {
    FakeEventSource.instances = []
    globalThis.EventSource = class extends FakeEventSource {
      constructor(url: string) {
        super(url)
        FakeEventSource.instances.push(this)
      }
    } as unknown as typeof EventSource
  }

  /** 最近构造的实例（openLogStream 刚构造的那个）。 */
  static latest(): FakeEventSource {
    const src = FakeEventSource.instances[FakeEventSource.instances.length - 1]
    if (!src) throw new Error('无 EventSource 实例（openLogStream 未构造）')
    return src
  }

  constructor(url: string) {
    this.url = url
  }

  addEventListener(name: string, cb: (ev: unknown) => void): void {
    const set = this.listeners.get(name) ?? new Set()
    set.add(cb)
    this.listeners.set(name, set)
  }

  /** 派发命名事件（data 序列化为 JSON——SSE data 载荷形态）。 */
  dispatch(name: string, data: unknown): void {
    const ev = { data: JSON.stringify(data) }
    for (const cb of this.listeners.get(name) ?? []) cb(ev)
  }

  dispatchOpen(): void {
    for (const cb of this.listeners.get('open') ?? []) cb({})
  }

  dispatchError(): void {
    for (const cb of this.listeners.get('error') ?? []) cb({})
  }

  close(): void {
    this.closed = true
  }
}

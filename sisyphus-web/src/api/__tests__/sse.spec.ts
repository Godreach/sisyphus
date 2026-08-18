// 日志 SSE 客户端行为测试（票 B4-T4，ADR-0013）：只测外部行为——URL 构造、
// 事件解析、ANSI 剥离、断线重连语义（EventSource 原生重连由替身驱动）、
// 终态事件关流。用替身 EventSource 驱动 `openLogStream`，不测内部结构。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  buildLogStreamUrl,
  openLogStream,
  parseLogEvent,
  stripAnsi,
  type LogStreamConnectionStatus,
  type LogStreamEvent,
} from '@/api/sse'
import { FakeEventSource } from '@/test/fakeEventSource'

function lastSource(): FakeEventSource {
  return FakeEventSource.latest()
}

describe('buildLogStreamUrl（ADR-0013 定位语义）', () => {
  it('按 build/job/attempt 构造 + from=seq 起播', () => {
    expect(buildLogStreamUrl('my proj', 'release', 7, 'compile', 2)).toBe(
      '/api/v1/projects/my%20proj/pipelines/release/builds/7/jobs/compile/attempts/2/logs/stream?from=0',
    )
    expect(buildLogStreamUrl('p', 'pl', 1, 'lint', 1, 42)).toBe(
      '/api/v1/projects/p/pipelines/pl/builds/1/jobs/lint/attempts/1/logs/stream?from=42',
    )
  })
})

describe('parseLogEvent（SSE data 解析）', () => {
  it('输出块带 stream 标记', () => {
    const ev = parseLogEvent('{"type":"output","seq":5,"stream":"stderr","text":"err line"}')
    expect(ev).toEqual({ type: 'output', seq: 5, stream: 'stderr', text: 'err line' })
  })

  it('步骤 start 含命令回显 / end 含退出码与耗时', () => {
    const start = parseLogEvent(
      '{"type":"step_start","seq":3,"step":0,"name":"build","command":"cargo build","started_at":1000}',
    )
    expect(start).toEqual({
      type: 'step_start',
      seq: 3,
      step: 0,
      name: 'build',
      command: 'cargo build',
      started_at: 1000,
    })
    const end = parseLogEvent(
      '{"type":"step_end","seq":9,"step":0,"exit_code":0,"duration_ms":1200}',
    )
    expect(end).toEqual({ type: 'step_end', seq: 9, step: 0, exit_code: 0, duration_ms: 1200 })
  })

  it('截断事件带上限 / 终态事件带状态', () => {
    expect(parseLogEvent('{"type":"truncated","seq":7,"limit_bytes":52428800}')).toEqual({
      type: 'truncated',
      seq: 7,
      limit_bytes: 52428800,
    })
    expect(parseLogEvent('{"type":"job_end","seq":12,"status":"succeeded","exit_code":0}')).toEqual({
      type: 'job_end',
      seq: 12,
      status: 'succeeded',
      exit_code: 0,
    })
  })

  it('非法载荷返回 null（不炸）', () => {
    expect(parseLogEvent('not json')).toBeNull()
    expect(parseLogEvent('{"type":"unknown","seq":1}')).toBeNull()
    expect(parseLogEvent('{"seq":1}')).toBeNull()
    expect(parseLogEvent('{"type":"output"}')).toBeNull()
    expect(parseLogEvent('{"type":"output","seq":"x","stream":"stdout","text":"y"}')).toBeNull()
  })
})

describe('stripAnsi（ANSI 剥离，ADR-0013）', () => {
  it('剥离颜色/光标/OSC 序列，保留换行与文本', () => {
    expect(stripAnsi('\x1b[31mred\x1b[0m')).toBe('red')
    expect(stripAnsi('\x1b[1;32mgreen\x1b[m')).toBe('green')
    expect(stripAnsi('a\x1b[2Kb')).toBe('ab')
    expect(stripAnsi('\x1b]0;title\x07visible')).toBe('visible')
    expect(stripAnsi('line1\nline2\tcol')).toBe('line1\nline2\tcol')
  })
})

describe('openLogStream（SSE 流接线）', () => {
  let events: LogStreamEvent[]
  let statuses: LogStreamConnectionStatus[]
  const onEvent = (e: LogStreamEvent) => events.push(e)
  const onStatus = (s: LogStreamConnectionStatus) => statuses.push(s)

  beforeEach(() => {
    events = []
    statuses = []
    FakeEventSource.install()
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('打开即 connecting，open 后转发事件并进入 open', () => {
    const conn = openLogStream('/logs/stream', onEvent, onStatus)
    expect(statuses).toEqual(['connecting'])
    const src = lastSource()
    expect(src.url).toBe('/logs/stream')
    expect(statuses).toEqual(['connecting'])

    src.dispatchOpen()
    expect(statuses).toContain('open')

    src.dispatch('output', { type: 'output', seq: 1, stream: 'stdout', text: 'hi' })
    expect(events).toHaveLength(1)
    expect(events[0]).toMatchObject({ type: 'output', text: 'hi' })

    conn.close()
  })

  it('首连失败（未 open 过）→ degraded 退化态并关流（端点未交付）', () => {
    const conn = openLogStream('/logs/stream', onEvent, onStatus)
    const src = lastSource()
    src.dispatchError()
    expect(statuses).toContain('degraded')
    expect(src.closed).toBe(true)
    conn.close()
  })

  it('已 open 过再断线 → reconnecting（原生 EventSource 自动重连续传）', () => {
    const conn = openLogStream('/logs/stream', onEvent, onStatus)
    const src = lastSource()
    src.dispatchOpen()
    src.dispatchError()
    expect(statuses).toContain('reconnecting')
    expect(src.closed).toBe(false)
    conn.close()
  })

  it('终态事件（job_end）送达即关流（ADR-0013）', () => {
    const conn = openLogStream('/logs/stream', onEvent, onStatus)
    const src = lastSource()
    src.dispatch('job_end', { type: 'job_end', seq: 12, status: 'succeeded', exit_code: 0 })
    expect(events).toHaveLength(1)
    expect(events[0]).toMatchObject({ type: 'job_end', status: 'succeeded' })
    expect(statuses).toContain('closed')
    expect(src.closed).toBe(true)
    // 关流后再收事件不转发。
    src.dispatch('output', { type: 'output', seq: 13, stream: 'stdout', text: 'x' })
    expect(events).toHaveLength(1)
    conn.close()
  })

  it('close 主动关流 → closed', () => {
    const conn = openLogStream('/logs/stream', onEvent, onStatus)
    const src = lastSource()
    conn.close()
    expect(statuses).toContain('closed')
    expect(src.closed).toBe(true)
  })
})

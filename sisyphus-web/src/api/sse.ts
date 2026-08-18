// 构建日志 SSE 客户端（ADR-0013，票 B4-T4）。
//
// - 单一 SSE 端点 per job：`from=<seq>` 起播（缺省 0，先从 DB 补历史、再接
//   事件总线实时尾随）；浏览器原生 EventSource 断线自动重连带 `Last-Event-ID`
//   （即 seq）原地续传——本客户端只构造 URL 与消费事件，重连语义交给原生。
// - 流结构（ADR-0013）：带类型的事件流，stdout/stderr 合流；流元素 = 输出块
//   （带 stream 标记）+ 步骤生命周期事件（step start 含命令回显 / step end 含
//   退出码与耗时）+ 截断标记，单一有序序列按到达顺序交织；任务终态事件送达
//   即关流。SSE 以命名事件（`event: <type>`）传输、`id: <seq>` 承载断线续传
//   游标。
// - 端点路径：本批 server 尚未交付日志端点（B4 纯前端消费既有契约，缺端点
//   走「退化态 + 显式标注」纪律，Spec B4 §Out of Scope）。路径按 ADR-0013
//   定位语义（build, job, attempt, seq）构造：缺省 from=0；首连失败即视为
//   端点未交付（degraded），由调用侧显式标注。

import { withQuery } from './http'

/** 输出流标记（stdout/stderr 合流，ADR-0013）。 */
export type LogStream = 'stdout' | 'stderr'

/** 步骤生命周期事件（step start 含命令回显 / step end 含退出码与耗时）。 */
export interface StepStartEvent {
  seq: number
  /** 步骤序号（从 0 起）。 */
  step: number
  /** 步骤名（可为空——未命名的 shell 步骤）。 */
  name: string
  /** 命令回显（Agent 始终回显步骤命令行进日志，ADR-0013）。 */
  command: string
  /** 步骤开始时刻（Unix 毫秒）。 */
  started_at: number
}

export interface StepEndEvent {
  seq: number
  /** 步骤序号（与 step start 对应）。 */
  step: number
  /** 退出码（可空）。 */
  exit_code: number | null
  /** 耗时（毫秒）。 */
  duration_ms: number
}

/** 截断标记：per-job 日志达上限（Server 全局配置，默认 50 MB），Agent 丢弃
 *  超限输出并在流内插入本事件——截断不判败，UI 显著标注（ADR-0013）。 */
export interface TruncatedEvent {
  seq: number
  /** 触发截断的日志上限（字节）。 */
  limit_bytes: number
}

/** 任务终态事件：送达并 flush 后关流（ADR-0013）。 */
export interface JobEndEvent {
  seq: number
  /** 任务终态（succeeded/failed/cancelled/timeout/aborted）。 */
  status: string
  /** 退出码（可空）。 */
  exit_code: number | null
}

/** 输出块（带 stream 标记）。 */
export interface OutputEvent {
  seq: number
  /** stdout/stderr 合流标记。 */
  stream: LogStream
  /** 输出文本（原始字节，含 ANSI 色码；渲染时剥离，ADR-0013）。 */
  text: string
}

/** 归一化的流事件（单一有序序列）。 */
export type LogStreamEvent =
  | ({ type: 'output' } & OutputEvent)
  | ({ type: 'step_start' } & StepStartEvent)
  | ({ type: 'step_end' } & StepEndEvent)
  | ({ type: 'truncated' } & TruncatedEvent)
  | ({ type: 'job_end' } & JobEndEvent)

/** SSE 命名事件名 → 归一化事件类型。 */
const EVENT_TYPES = [
  'output',
  'step_start',
  'step_end',
  'truncated',
  'job_end',
] as const

type EventType = (typeof EVENT_TYPES)[number]

/**
 * 解析 SSE `data` 载荷为归一化流事件。形态非法（缺字段/类型错）返回 `null`
 * ——调用侧忽略（SSE 流演进时旧客户端跳过未知/损坏事件，不炸）。
 */
export function parseLogEvent(data: string): LogStreamEvent | null {
  let raw: unknown
  try {
    raw = JSON.parse(data)
  } catch {
    return null
  }
  if (typeof raw !== 'object' || raw === null) return null
  const obj = raw as Record<string, unknown>
  const type = obj.type
  if (typeof type !== 'string' || !(EVENT_TYPES as readonly string[]).includes(type)) {
    return null
  }
  const seq = obj.seq
  if (typeof seq !== 'number' || !Number.isFinite(seq)) return null

  switch (type as EventType) {
    case 'output': {
      const stream = obj.stream
      const text = obj.text
      if (stream !== 'stdout' && stream !== 'stderr') return null
      if (typeof text !== 'string') return null
      return { type: 'output', seq, stream, text }
    }
    case 'step_start': {
      const step = obj.step
      if (typeof step !== 'number' || !Number.isInteger(step)) return null
      return {
        type: 'step_start',
        seq,
        step,
        name: typeof obj.name === 'string' ? obj.name : '',
        command: typeof obj.command === 'string' ? obj.command : '',
        started_at: typeof obj.started_at === 'number' ? obj.started_at : 0,
      }
    }
    case 'step_end': {
      const step = obj.step
      if (typeof step !== 'number' || !Number.isInteger(step)) return null
      return {
        type: 'step_end',
        seq,
        step,
        exit_code: typeof obj.exit_code === 'number' ? obj.exit_code : null,
        duration_ms: typeof obj.duration_ms === 'number' ? obj.duration_ms : 0,
      }
    }
    case 'truncated':
      return {
        type: 'truncated',
        seq,
        limit_bytes: typeof obj.limit_bytes === 'number' ? obj.limit_bytes : 0,
      }
    case 'job_end':
      return {
        type: 'job_end',
        seq,
        status: typeof obj.status === 'string' ? obj.status : '',
        exit_code: typeof obj.exit_code === 'number' ? obj.exit_code : null,
      }
    default:
      return null
  }
}

/** 剥离 ANSI/VT100 转义（颜色、光标、清屏、OSC 标题/超链接）与 C0 控制符。
 *  保留换行/制表（日志渲染需要）；日志字节原样存储、纯文本渲染时剥离
 *  （ADR-0013）。 */
export function stripAnsi(text: string): string {
  return text.replace(
    /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9A-Z]|[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g,
    '',
  )
}

/**
 * 日志 SSE 端点 URL（ADR-0013 定位语义：build, job, attempt, seq）：
 * `GET /api/v1/projects/{project}/pipelines/{pipeline}/builds/{number}/
 *  jobs/{job}/attempts/{attempt}/logs/stream?from=<seq>`。`from` 缺省 0
 * （起播从 DB 补历史）；断线续传由原生 EventSource 携 `Last-Event-ID` 完成。
 */
export function buildLogStreamUrl(
  project: string,
  pipeline: string,
  buildNumber: number,
  job: string,
  attempt: number,
  from = 0,
): string {
  const enc = (s: string) => encodeURIComponent(s)
  const path =
    `/api/v1/projects/${enc(project)}/pipelines/${enc(pipeline)}` +
    `/builds/${buildNumber}/jobs/${enc(job)}/attempts/${attempt}/logs/stream`
  return withQuery(path, { from })
}

/** 连接状态（BuildLogView 消费展示；degraded = 端点未交付/首连失败）。 */
export type LogStreamConnectionStatus =
  | 'connecting'
  | 'open'
  | 'reconnecting'
  | 'closed'
  | 'degraded'

/** 打开的日志流句柄（调用侧负责 close）。 */
export interface LogStreamConnection {
  close: () => void
}

/**
 * 打开日志 SSE 流：构造原生 EventSource 并接线。
 *
 * - 断线自动重连是原生语义（`Last-Event-ID` 续传），本层不重复实现。
 * - 首连失败（从未 open 且已收到 error）视为「端点未交付/不可达」的退化态：
 *   关闭流并回调 `onStatus('degraded')`——调用侧显式标注（Spec B4 缺端点纪律）；
 *   已 open 过再断线回调 `reconnecting`（原生重连中）。
 * - 任务终态事件（job_end）送达即关流（ADR-0013），回调 `onStatus('closed')`。
 * - 测试以 EventSource 替身驱动（jsdom 无原生实现，Spec B4 测试缝）。
 */
export function openLogStream(
  url: string,
  onEvent: (event: LogStreamEvent) => void,
  onStatus: (status: LogStreamConnectionStatus) => void,
): LogStreamConnection {
  let closed = false
  let everOpened = false
  let source: EventSource

  onStatus('connecting')
  source = new EventSource(url)

  source.addEventListener('open', () => {
    if (closed) return
    everOpened = true
    onStatus('open')
  })

  source.addEventListener('error', () => {
    if (closed) return
    if (!everOpened) {
      // 首连失败：端点未交付/不可达 → 显式退化态并关流（避免无限重连）。
      closed = true
      source.close()
      onStatus('degraded')
      return
    }
    // 已开过流：断线由原生 EventSource 自动重连（携 Last-Event-ID 续传）。
    onStatus('reconnecting')
  })

  for (const name of EVENT_TYPES) {
    source.addEventListener(name, (ev) => {
      if (closed) return
      const event = parseLogEvent((ev as MessageEvent).data)
      if (!event) return
      if (event.type === 'job_end') {
        // 终态事件送达即关流（ADR-0013）。
        closed = true
        source.close()
        onEvent(event)
        onStatus('closed')
        return
      }
      onEvent(event)
    })
  }

  return {
    close() {
      if (closed) return
      closed = true
      source.close()
      onStatus('closed')
    },
  }
}

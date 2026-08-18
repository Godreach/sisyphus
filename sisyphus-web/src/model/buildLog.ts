// 构建日志视图模型（票 B4-T4，ADR-0013）：把 SSE 流事件（输出块 + 步骤
// 生命周期 + 截断）累积为「按步骤折叠渲染」的视图结构。
//
// - 单一有序序列按到达顺序交织；步骤开始前的输出归入 preamble（如 checkout
//   前的工作区准备输出），步骤内输出挂在步骤下（带 stdout/stderr stream 标记）。
// - ANSI 在入模型时剥离（日志字节原样存储、纯文本渲染时剥离，ADR-0013）。
// - 截断事件显著标注（per-job 日志上限，截断不判败）。
// - 纯逻辑、无依赖，行为测试直接驱动（步骤折叠/截断/终态），不经组件。

import type { LogStreamEvent } from '@/api/sse'
import { stripAnsi } from '@/api/sse'

/** 一条输出行（ANSI 已剥离、带合流 stream 标记）。 */
export interface LogOutputLine {
  stream: 'stdout' | 'stderr'
  text: string
}

/** 视图层的一个步骤（含其输出块、命令回显与结束信息）。 */
export interface LogStep {
  /** 步骤序号（从 0 起）。 */
  index: number
  /** 步骤名（可为空——未命名的 shell 步骤）。 */
  name: string
  /** 命令回显（Agent 始终回显步骤命令行，ADR-0013）。 */
  command: string
  /** 步骤内输出行（stdout/stderr 合流保序）。 */
  lines: LogOutputLine[]
  /** 退出码（step end 前为空）。 */
  exitCode: number | null
  /** 耗时毫秒（step end 前为空）。 */
  durationMs: number | null
  /** 步骤开始时刻（Unix 毫秒）。 */
  startedAt: number | null
  /** 折叠态（默认展开；用户点击步骤头切换）。 */
  collapsed: boolean
}

/** 累积后的日志视图模型。 */
export interface BuildLogModel {
  /** 步骤开始前的输出（工作区准备等）。 */
  preamble: LogOutputLine[]
  /** 步骤（按序号序）。 */
  steps: LogStep[]
  /** 截断时的日志上限字节（null = 未截断）。 */
  truncatedAt: number | null
  /** 任务终态事件已送达（流已关，ADR-0013）。 */
  ended: boolean
}

/** 新建空模型（每次打开新流时重置）。 */
export function createLogModel(): BuildLogModel {
  return {
    preamble: [],
    steps: [],
    truncatedAt: null,
    ended: false,
  }
}

/** 按步骤序号找步骤视图（不存在则按序插入——步骤事件到达顺序即渲染序）。 */
function stepAt(model: BuildLogModel, index: number): LogStep {
  let found = model.steps.find((s) => s.index === index)
  if (found) return found
  found = {
    index,
    name: '',
    command: '',
    lines: [],
    exitCode: null,
    durationMs: null,
    startedAt: null,
    collapsed: false,
  }
  model.steps.push(found)
  model.steps.sort((a, b) => a.index - b.index)
  return found
}

/** 追加一条流事件到视图模型（非破坏性——测试可复现累积）。 */
export function appendEvent(model: BuildLogModel, event: LogStreamEvent): void {
  switch (event.type) {
    case 'output': {
      const line: LogOutputLine = { stream: event.stream, text: stripAnsi(event.text) }
      const last = model.steps[model.steps.length - 1]
      // 有步骤则挂在最后一步；否则归 preamble。
      if (last) last.lines.push(line)
      else model.preamble.push(line)
      break
    }
    case 'step_start': {
      const step = stepAt(model, event.step)
      step.name = event.name
      step.command = event.command
      step.startedAt = event.started_at
      break
    }
    case 'step_end': {
      const step = stepAt(model, event.step)
      step.exitCode = event.exit_code
      step.durationMs = event.duration_ms
      break
    }
    case 'truncated':
      model.truncatedAt = event.limit_bytes
      break
    case 'job_end':
      model.ended = true
      break
  }
}

/** 切换步骤折叠态（返回新状态）。 */
export function toggleStep(model: BuildLogModel, index: number): boolean {
  const step = model.steps.find((s) => s.index === index)
  if (!step) return false
  step.collapsed = !step.collapsed
  return step.collapsed
}

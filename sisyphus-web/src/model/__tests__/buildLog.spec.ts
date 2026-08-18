// 构建日志视图模型测试（票 B4-T4，ADR-0013）：只测外部行为——事件累积为
// 按步骤折叠的视图结构、ANSI 剥离、截断标注、终态、输出归属（步骤前
// preamble / 步骤内）。

import { describe, expect, it } from 'vitest'

import {
  appendEvent,
  createLogModel,
  toggleStep,
} from '@/model/buildLog'
import type { LogStreamEvent } from '@/api/sse'

function output(seq: number, stream: 'stdout' | 'stderr', text: string): LogStreamEvent {
  return { type: 'output', seq, stream, text }
}
function stepStart(seq: number, step: number, name: string, command: string): LogStreamEvent {
  return { type: 'step_start', seq, step, name, command, started_at: seq * 1000 }
}
function stepEnd(seq: number, step: number, exitCode: number, durationMs: number): LogStreamEvent {
  return { type: 'step_end', seq, step, exit_code: exitCode, duration_ms: durationMs }
}

describe('BuildLogModel（事件累积）', () => {
  it('步骤开始前的输出归 preamble；进入步骤后挂到步骤下', () => {
    const model = createLogModel()
    appendEvent(model, output(1, 'stdout', 'preparing workspace'))
    appendEvent(model, stepStart(2, 0, 'build', 'cargo build'))
    appendEvent(model, output(3, 'stdout', 'compiling...'))
    appendEvent(model, output(4, 'stderr', 'warning: unused'))
    appendEvent(model, stepEnd(5, 0, 0, 1200))

    expect(model.preamble.map((l) => l.text)).toEqual(['preparing workspace'])
    expect(model.steps).toHaveLength(1)
    const step = model.steps[0]!
    expect(step.name).toBe('build')
    expect(step.command).toBe('cargo build')
    expect(step.lines).toEqual([
      { stream: 'stdout', text: 'compiling...' },
      { stream: 'stderr', text: 'warning: unused' },
    ])
    expect(step.exitCode).toBe(0)
    expect(step.durationMs).toBe(1200)
    expect(model.ended).toBe(false)
  })

  it('ANSI 色码在入模型时剥离（纯文本渲染，ADR-0013）', () => {
    const model = createLogModel()
    appendEvent(model, output(1, 'stdout', '\x1b[32mok\x1b[0m done'))
    expect(model.preamble[0]?.text).toBe('ok done')
  })

  it('截断事件显著标注上限（截断不判败）', () => {
    const model = createLogModel()
    appendEvent(model, output(1, 'stdout', 'before'))
    appendEvent(model, { type: 'truncated', seq: 2, limit_bytes: 52_428_800 })
    appendEvent(model, output(3, 'stdout', 'after'))
    expect(model.truncatedAt).toBe(52_428_800)
    // 截断后输出仍正常累积（截断不关流）。
    expect(model.preamble.map((l) => l.text)).toEqual(['before', 'after'])
  })

  it('终态事件置 ended（流关闭语义）', () => {
    const model = createLogModel()
    appendEvent(model, { type: 'job_end', seq: 1, status: 'succeeded', exit_code: 0 })
    expect(model.ended).toBe(true)
  })

  it('步骤乱序到达仍按序号排序（流按到达序交织）', () => {
    const model = createLogModel()
    // 步骤 1 先出输出，步骤 0 后 start——步骤卡按序号排序展示。
    appendEvent(model, stepStart(1, 1, 'lint', 'cargo clippy'))
    appendEvent(model, output(2, 'stdout', 'lint out'))
    appendEvent(model, stepStart(3, 0, 'build', 'cargo build'))
    expect(model.steps.map((s) => s.index)).toEqual([0, 1])
  })

  it('toggleStep 切换折叠态并返回新状态', () => {
    const model = createLogModel()
    appendEvent(model, stepStart(1, 0, 'build', 'cargo build'))
    expect(model.steps[0]?.collapsed).toBe(false)
    expect(toggleStep(model, 0)).toBe(true)
    expect(model.steps[0]?.collapsed).toBe(true)
    expect(toggleStep(model, 0)).toBe(false)
    expect(toggleStep(model, 99)).toBe(false)
  })
})

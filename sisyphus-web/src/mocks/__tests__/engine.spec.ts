// 动态构建生命周期引擎对账（票 #107：SSE 日志流 AC 的 mock 侧对账——
// 「运行中实时推送至终态」由本引擎驱动 MockEventSource，vitest 以假定时器
// 推进节奏，断言日志事件序列（step_start / output / step_end / job_end、
// seq 连续）、产物落盘、取消与 from_failed 重跑语义。只测 mock 引擎外部
// 行为（事件序列与状态终态），不测内部实现。

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  cancelBuild,
  dynamicArtifacts,
  dynamicBuild,
  logHistory,
  rerunFromFailed,
  triggerBuild,
} from '@/mocks/engine'
import type { LogStreamEvent } from '@/api/sse'

/** 反复推进假定时器直到构建终态（或步数上限——防死循环）。 */
async function advanceUntilTerminal(project: string, pipeline: string, number: number): Promise<void> {
  for (let i = 0; i < 400; i++) {
    const b = dynamicBuild(project, pipeline, number)
    if (b == null) throw new Error('build missing')
    if (b.status === 'succeeded' || b.status === 'failed' || b.status === 'cancelled') return
    await vi.advanceTimersByTimeAsync(500)
  }
  throw new Error('build did not reach terminal state in time')
}

describe('mock 动态构建生命周期引擎（票 #107 SSE 流对账）', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('成功路径：排队→运行中→成功；日志事件 seq 连续、步骤生命周期与输出块交织、产物落盘', async () => {
    const acc = triggerBuild('web-app', 'release', { triggerBy: 'admin' })
    expect(acc).not.toBeNull()
    expect(acc!.status).toBe('queued')
    const number = acc!.number

    await advanceUntilTerminal('web-app', 'release', number)

    const build = dynamicBuild('web-app', 'release', number)
    expect(build!.status).toBe('succeeded')
    expect(build!.finishedAt).not.toBeNull()

    // 第一个任务（compile）日志：seq 连续、首事件 step_start、终事件
    // job_end(succeeded)、含 stdout 输出块与 step_end(0)。
    const events = logHistory('web-app', 'release', number, 'compile', 1)
    expect(events.length).toBeGreaterThan(0)
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i + 1))
    expect(events[0]!.type).toBe('step_start')
    expect(events[events.length - 1]!.type).toBe('job_end')
    expect((events[events.length - 1] as { status?: string }).status).toBe('succeeded')
    expect(events.some((e) => e.type === 'output' && e.stream === 'stdout')).toBe(true)
    const stepEnds = events.filter((e) => e.type === 'step_end') as Array<LogStreamEvent & { exit_code: number }>
    expect(stepEnds.length).toBeGreaterThan(0)
    expect(stepEnds.every((e) => e.exit_code === 0)).toBe(true)

    // 产物：package 任务声明的两个产物在成功后可下载（动态 artifacts）。
    const artifacts = dynamicArtifacts('web-app', 'release', number) ?? []
    const names = artifacts.map((a) => a.name)
    expect(names).toContain('app-linux-amd64.tar.gz')
    expect(names).toContain('checksums.txt')
    for (const a of artifacts) {
      expect(a.size).toBeGreaterThan(0)
      expect(a.sha256).toHaveLength(64)
    }
  })

  it('失败路径（FAIL=1）：最后任务失败、失败步骤 exit_code 1、job_end(failed)；from_failed 重跑同号 attempt+1', async () => {
    const acc = triggerBuild('web-app', 'main', { triggerBy: 'admin', params: { FAIL: '1' } })
    expect(acc).not.toBeNull()
    const number = acc!.number

    await advanceUntilTerminal('web-app', 'main', number)
    expect(dynamicBuild('web-app', 'main', number)!.status).toBe('failed')

    // 最后一个任务的 job_end(failed) + 失败 detail。
    const jobs = dynamicBuild('web-app', 'main', number)!.jobs
    const lastJob = jobs[jobs.length - 1]!
    expect(lastJob.status).toBe('failed')
    expect(lastJob.exit_code).toBe(1)
    const events = logHistory('web-app', 'main', number, lastJob.name, 1)
    const last = events[events.length - 1] as LogStreamEvent & { status?: string }
    expect(last.type).toBe('job_end')
    expect(last.status).toBe('failed')
    // fail-fast：日志里不应有后续任务的 step_start（后续任务 skipped 无日志）。

    // from_failed 重跑：同号 attempt+1，新 attempt 日志从 seq 1 起独立成流。
    const rerun = rerunFromFailed('web-app', 'main', number, 'admin')
    expect(rerun).not.toBeNull()
    expect(rerun!.number).toBe(number)
    expect(rerun!.attempt).toBe(2)
    const attempt2 = logHistory('web-app', 'main', number, lastJob.name, 2)
    // attempt 2 刚建：尚无事件或从 1 起连续（不与 attempt 1 混流）。
    expect(attempt2.map((e) => e.seq)).toEqual(attempt2.map((_, i) => i + 1))
  })

  it('取消路径：运行中取消 → 构建与活跃任务 cancelled、排队任务 skipped、活跃流推 job_end', async () => {
    const acc = triggerBuild('api-gateway', 'integration', { triggerBy: 'admin' })
    expect(acc).not.toBeNull()
    const number = acc!.number

    // 推进到运行中（排队 2.5s + 若干输出块）。
    await vi.advanceTimersByTimeAsync(4000)
    const running = dynamicBuild('api-gateway', 'integration', number)
    expect(running!.status).toBe('running')
    const activeJob = running!.jobs.find((j) => j.status === 'running')!

    const accepted = cancelBuild('api-gateway', 'integration', number)
    expect(accepted!.status).toBe('cancelled')

    const build = dynamicBuild('api-gateway', 'integration', number)
    expect(build!.cancelledAt).not.toBeNull()
    expect(build!.jobs.find((j) => j.name === activeJob.name)!.status).toBe('cancelled')
    // 取消时仍排队的任务 → skipped。
    expect(build!.jobs.some((j) => j.status === 'skipped')).toBe(true)
    // 活跃日志流收到 job_end(cancelled) 收尾。
    const events = logHistory('api-gateway', 'integration', number, activeJob.name, activeJob.attempt)
    const last = events[events.length - 1] as LogStreamEvent & { status?: string }
    expect(last.type).toBe('job_end')
    expect(last.status).toBe('cancelled')
  })
})

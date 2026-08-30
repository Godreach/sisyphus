// 流水线统计 + 构建机资源指标契约测试（契约票 #102，ADR-0024 单一缝）：
// 经真实 http client 打 MSW handlers（fixture 即测试数据），断言契约票
// #102 冻结的聚合口径与「—」空数据路径。只测外部行为（响应形态与口径）。

import { afterEach, beforeAll, afterAll, describe, expect, it } from 'vitest'

import { agentsApi, pipelinesApi } from '@/api/client'
import { ApiError } from '@/api/http'
import { server } from '@/mocks/node'

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

/** 与 db.pipelineStatsFrom 同式的服务端口径重算（测试侧独立实现，防双错同源）。 */
function expectRateShape(rate: number | null, succeeded: number, terminal: number): void {
  if (terminal === 0) expect(rate).toBeNull()
  else expect(rate).toBeCloseTo(Math.round((succeeded / terminal) * 1000) / 10, 6)
}

describe('GET /projects/:name/pipelines/:pipeline/stats（契约票 #102）', () => {
  it('按冻结口径返回聚合形态（窗口/终态计数/成功率一位小数/平均耗时整数毫秒/latest_build）', async () => {
    const stats = await pipelinesApi.stats('web-app', 'main', 20)
    expect(stats.window).toBeLessThanOrEqual(20)
    expect(stats.total_builds).toBeGreaterThanOrEqual(stats.window)
    expect(stats.terminal_count).toBeLessThanOrEqual(stats.window)
    expect(stats.succeeded_count).toBeLessThanOrEqual(stats.terminal_count)
    expectRateShape(stats.success_rate, stats.succeeded_count, stats.terminal_count)
    if (stats.avg_duration_ms != null) {
      expect(Number.isInteger(stats.avg_duration_ms)).toBe(true)
      // 零时长构建（finished == started）合法，平均耗时可为 0。
      expect(stats.avg_duration_ms).toBeGreaterThanOrEqual(0)
    } else {
      // 无可测耗时样本时终态计数也应为 0（口径自洽）。
      expect(stats.terminal_count).toBe(0)
    }
    if (stats.latest_build != null) {
      expect(typeof stats.latest_build.number).toBe('number')
      expect(['queued', 'running', 'succeeded', 'failed', 'cancelled', 'timeout']).toContain(
        stats.latest_build.status,
      )
    }
  })

  it('零构建流水线走「—」路径：success_rate / avg_duration_ms / latest_build 均 null', async () => {
    const stats = await pipelinesApi.stats('empty-repo', 'main', 20)
    expect(stats.total_builds).toBe(0)
    expect(stats.window).toBe(0)
    expect(stats.terminal_count).toBe(0)
    expect(stats.succeeded_count).toBe(0)
    expect(stats.success_rate).toBeNull()
    expect(stats.avg_duration_ms).toBeNull()
    expect(stats.latest_build).toBeNull()
  })

  it('窗口越界取边界值（window=0 → 下边界 1；window=999 收敛到实际构建数且不超上界）', async () => {
    const low = await pipelinesApi.stats('web-app', 'main', 0)
    expect(low.window).toBe(1)
    const high = await pipelinesApi.stats('web-app', 'main', 999)
    // fixture 该流水线构建数 < 100：越上界请求收敛到全部构建数（≤100）。
    expect(high.window).toBe(high.total_builds)
    expect(high.window).toBeLessThanOrEqual(100)
  })

  it('不存在的流水线 404（与构建列表同语义）', async () => {
    const err = await pipelinesApi.stats('web-app', 'no-such', 20).then(
      () => null,
      (e: unknown) => e,
    )
    expect(err).toBeInstanceOf(ApiError)
    expect((err as ApiError).status).toBe(404)
  })
})

describe('AgentResponse 资源指标字段（契约票 #102 v1 裁定）', () => {
  it('清单含 cpu_usage / memory_usage；无上报能力的旧版本 Agent 为 null（「—」路径）', async () => {
    const agents = await agentsApi.list()
    expect(agents.length).toBeGreaterThan(0)
    for (const agent of agents) {
      expect(agent).toHaveProperty('cpu_usage')
      expect(agent).toHaveProperty('memory_usage')
      if (agent.cpu_usage != null) {
        expect(agent.cpu_usage).toBeGreaterThanOrEqual(0)
        expect(agent.cpu_usage).toBeLessThanOrEqual(100)
      }
      if (agent.memory_usage != null) {
        expect(agent.memory_usage).toBeGreaterThanOrEqual(0)
        expect(agent.memory_usage).toBeLessThanOrEqual(100)
      }
    }
    // fixture 裁定：v1.4 起具备上报能力——更旧版本（build-05/07）为 null。
    const legacy = agents.filter((a) => a.cpu_usage == null).map((a) => a.name)
    expect(legacy).toContain('build-05')
    expect(legacy).toContain('build-07')
  })
})

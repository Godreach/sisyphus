// 动态构建生命周期引擎（票 #101，ADR-0013/0024）：触发/重跑产生的构建在
// 内存里按节奏推进——排队（约 2.5s）→ 运行中（逐任务：步骤生命周期事件 +
// 输出块按节奏推送）→ 终态（成功 / 失败，含产物上传）。
//
// - REST handlers（builds list/detail/artifacts）读引擎内存态；日志 SSE 流
//   不经 MSW（EventSource 不走 fetch，见 eventSource.ts），由本引擎的订阅
//   接口直接驱动 MockEventSource。
// - 失败演示：触发时参数 `FAIL=1` 或 pipeline 名为 `nightly` 的构建在最后
//   一个任务失败（供 from_failed 重跑闭环演示）。
// - 定时器在 node（vitest）环境下 unref，不阻塞测试进程退出。

import type {
  ArtifactResponse,
  BuildAcceptedResponse,
  BuildDetailResponse,
  BuildStatusDto,
  BuildSummaryResponse,
  JobStatusDto,
  JobViewDto,
  TriggerSourceDto,
} from '@/api/types'
import type { LogStreamEvent } from '@/api/sse'
import { AGENTS, findPipeline, mulberry32, nextBuildNumber } from './db'

/** 动态构建内存态。 */
interface DynJob {
  name: string
  /** 所属阶段序（从 0 起，触发时从定义快照固定）。 */
  stageIndex: number
  status: JobStatusDto
  attempt: number
  started_at: number | null
  finished_at: number | null
  exit_code: number | null
  detail: string | null
  agent_id: number | null
}

interface DynBuild {
  project: string
  pipeline: string
  number: number
  buildId: number
  attempt: number
  status: BuildStatusDto
  trigger: TriggerSourceDto
  triggerBy: string
  createdAt: number
  startedAt: number | null
  finishedAt: number | null
  cancelledAt: number | null
  jobs: DynJob[]
  artifacts: ArtifactResponse[]
  /** 日志事件流（key = `${job}:${attempt}`，seq 从 1 起连续递增）。 */
  logs: Map<string, LogStreamEvent[]>
  cancelled: boolean
  failPlan: boolean
  /** 阶段名（触发时从定义快照，后续定义编辑不影响本次运行）。 */
  stageNames: string[]
}

const BUILDS = new Map<string, Map<number, DynBuild>>()
let nextBuildId = 900000

type LogSubscriber = (event: LogStreamEvent) => void
const LOG_SUBSCRIBERS = new Map<string, Set<LogSubscriber>>()

function buildKey(project: string, pipeline: string, number: number): string {
  return `${project}/${pipeline}#${number}`
}

function logKey(job: string, attempt: number): string {
  return `${job}:${attempt}`
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    const t = setTimeout(resolve, ms)
    ;(t as unknown as { unref?: () => void }).unref?.()
  })
}

/** 动态构建任务占/释放 Agent 槽位（回写 fixture AGENTS.active_jobs）：
 *  概览「在途任务 = 槽位占用」与构建机页当前任务对动态构建同口径
 *  （契约票 #104：触发后刷新快照，在途/队列计数真实变化）。 */
function adjustAgentLoad(agentId: number | null, delta: number): void {
  if (agentId == null) return
  const agent = AGENTS[agentId - 1]
  if (agent == null) return
  agent.active_jobs = Math.max(0, agent.active_jobs + delta)
}

function appendLog(build: DynBuild, job: string, attempt: number, event: LogStreamEvent): void {
  const key = logKey(job, attempt)
  let log = build.logs.get(key)
  if (log == null) {
    log = []
    build.logs.set(key, log)
  }
  event.seq = log.length + 1
  log.push(event)
  for (const sub of LOG_SUBSCRIBERS.get(`${buildKey(build.project, build.pipeline, build.number)}:${key}`) ?? []) {
    sub(event)
  }
}

/** 订阅某任务日志流（MockEventSource 消费）；返回退订函数。 */
export function subscribeLogs(
  project: string,
  pipeline: string,
  number: number,
  job: string,
  attempt: number,
  subscriber: LogSubscriber,
): () => void {
  const key = `${buildKey(project, pipeline, number)}:${logKey(job, attempt)}`
  let subs = LOG_SUBSCRIBERS.get(key)
  if (subs == null) {
    subs = new Set()
    LOG_SUBSCRIBERS.set(key, subs)
  }
  subs.add(subscriber)
  return () => {
    subs?.delete(subscriber)
  }
}

/** 已落盘的历史日志事件（SSE 断线重连 `from=` 续传消费）。 */
export function logHistory(
  project: string,
  pipeline: string,
  number: number,
  job: string,
  attempt: number,
): LogStreamEvent[] {
  return BUILDS.get(`${project}/${pipeline}`)?.get(number)?.logs.get(logKey(job, attempt)) ?? []
}

// ---------------------------------------------------------------------------
// 受理 / 查询（handlers 消费面）
// ---------------------------------------------------------------------------

export function accepted(build: DynBuild): BuildAcceptedResponse {
  return {
    number: build.number,
    build_id: build.buildId,
    attempt: build.attempt,
    status: build.status,
  }
}

export function dynamicBuild(
  project: string,
  pipeline: string,
  number: number,
): DynBuild | null {
  return BUILDS.get(`${project}/${pipeline}`)?.get(number) ?? null
}

export function dynamicSummaries(project: string, pipeline: string): BuildSummaryResponse[] {
  const bucket = BUILDS.get(`${project}/${pipeline}`)
  if (bucket == null) return []
  return [...bucket.values()].map(summaryOf)
}

/** 全部动态构建概要（跨流水线；概览快照合并消费，契约票 #104：
 *  最近构建须含排队/运行中的动态态）。 */
export function allDynamicSummaries(): { project: string; pipeline: string; summary: BuildSummaryResponse }[] {
  const rows: { project: string; pipeline: string; summary: BuildSummaryResponse }[] = []
  for (const [key, bucket] of BUILDS) {
    const parts = key.split('/')
    const project = parts[0] as string
    const pipeline = parts[1] as string
    for (const b of bucket.values()) {
      rows.push({ project, pipeline, summary: summaryOf(b) })
    }
  }
  return rows
}

function summaryOf(b: DynBuild): BuildSummaryResponse {
  return {
    number: b.number,
    pipeline_name: b.pipeline,
    status: b.status,
    trigger: b.trigger,
    trigger_by: b.triggerBy,
    attempt: b.attempt,
    started_at: b.startedAt,
    finished_at: b.finishedAt,
    cancelled_at: b.cancelledAt,
  }
}

export function dynamicDetail(
  project: string,
  pipeline: string,
  number: number,
): BuildDetailResponse | null {
  const b = dynamicBuild(project, pipeline, number)
  if (b == null) return null
  const stages = b.stageNames.map((name, index) => ({
    index,
    name,
    jobs: b.jobs.filter((j) => j.stageIndex === index).map(toJobView),
  }))
  const elapsed =
    b.finishedAt != null && b.startedAt != null
      ? b.finishedAt - b.startedAt
      : b.startedAt != null
        ? Date.now() - b.startedAt
        : null
  return {
    number: b.number,
    pipeline_name: b.pipeline,
    status: b.status,
    trigger: b.trigger,
    trigger_by: b.triggerBy,
    attempt: b.attempt,
    started_at: b.startedAt,
    finished_at: b.finishedAt,
    cancelled_at: b.cancelledAt,
    elapsed_ms: elapsed,
    stages,
  }
}

function toJobView(j: DynJob): JobViewDto {
  return {
    name: j.name,
    status: j.status,
    attempt: j.attempt,
    started_at: j.started_at,
    finished_at: j.finished_at,
    exit_code: j.exit_code,
    allow_failure: false,
    detail: j.detail,
    agent_id: j.agent_id,
  }
}

export function dynamicArtifacts(
  project: string,
  pipeline: string,
  number: number,
): ArtifactResponse[] | null {
  const b = dynamicBuild(project, pipeline, number)
  return b?.artifacts ?? null
}

/** 触发构建：建内存态（排队）+ 调度推进。 */
export function triggerBuild(
  project: string,
  pipeline: string,
  options: { params?: Record<string, string>; triggerBy: string },
): BuildAcceptedResponse | null {
  const def = findPipeline(project, pipeline)
  if (def == null) return null
  const buildId = (nextBuildId += 1)
  const number = nextBuildNumber(project, pipeline)
  const failPlan =
    options.params?.['FAIL'] === '1' || pipeline === 'nightly'
  return accepted(createBuild(project, pipeline, number, buildId, 1, def, options.triggerBy, failPlan))
}

/** from_failed 重跑：同号 attempt+1（仅失败终态可；调用侧已校验）。 */
export function rerunFromFailed(
  project: string,
  pipeline: string,
  number: number,
  triggerBy: string,
): BuildAcceptedResponse | null {
  const prev = dynamicBuild(project, pipeline, number)
  const def = findPipeline(project, pipeline)
  if (def == null) return null
  const buildId = prev?.buildId ?? (nextBuildId += 1)
  const attempt = (prev?.attempt ?? 1) + 1
  const build = createBuild(project, pipeline, number, buildId, attempt, def, triggerBy, true)
  BUILDS.get(`${project}/${pipeline}`)?.set(number, build)
  return accepted(build)
}

function createBuild(
  project: string,
  pipeline: string,
  number: number,
  buildId: number,
  attempt: number,
  def: NonNullable<ReturnType<typeof findPipeline>>,
  triggerBy: string,
  failPlan: boolean,
): DynBuild {
  const stageNames = def.stages.map((s) => s.name)
  const jobs: DynJob[] = []
  for (let si = 0; si < def.stages.length; si++) {
    for (const job of def.stages[si]?.jobs ?? []) {
      jobs.push({
        name: job.name,
        stageIndex: si,
        status: 'queued',
        attempt,
        started_at: null,
        finished_at: null,
        exit_code: null,
        detail: null,
        agent_id: null,
      })
    }
  }
  const build: DynBuild = {
    project,
    pipeline,
    number,
    buildId,
    attempt,
    status: 'queued',
    trigger: 'manual',
    triggerBy,
    createdAt: Date.now(),
    startedAt: null,
    finishedAt: null,
    cancelledAt: null,
    jobs,
    artifacts: [],
    logs: new Map(),
    cancelled: false,
    failPlan,
    stageNames,
  }
  let bucket = BUILDS.get(`${project}/${pipeline}`)
  if (bucket == null) {
    bucket = new Map()
    BUILDS.set(`${project}/${pipeline}`, bucket)
  }
  bucket.set(number, build)
  void runBuild(build, def)
  return build
}

/** 取消：置取消态、当前任务 cancelled、未开始 skipped、活跃日志流推 job_end。 */
export function cancelBuild(project: string, pipeline: string, number: number): BuildAcceptedResponse | null {
  const b = dynamicBuild(project, pipeline, number)
  if (b == null) return null
  if (b.status === 'queued' || b.status === 'running') {
    b.cancelled = true
    b.status = 'cancelled'
    b.cancelledAt = Date.now()
    b.finishedAt = b.cancelledAt
    for (const job of b.jobs) {
      if (job.status === 'running') {
        job.status = 'cancelled'
        job.finished_at = b.cancelledAt
        adjustAgentLoad(job.agent_id, -1)
        appendLog(b, job.name, job.attempt, {
          seq: 0,
          type: 'job_end',
          status: 'cancelled',
          exit_code: null,
        })
      } else if (job.status === 'queued') {
        job.status = 'skipped'
      }
    }
  }
  return accepted(b)
}

/** 手动删动态构建（fixture 删除走 db.deleteBuildRecord）：终态才可删
 *  （调用侧已校验 409），从内存摘除。 */
export function removeBuild(project: string, pipeline: string, number: number): void {
  BUILDS.get(`${project}/${pipeline}`)?.delete(number)
}

// ---------------------------------------------------------------------------
// 运行时推进（setTimeout 节奏：任务 3–6s、输出块 500ms/块）
// ---------------------------------------------------------------------------

const STEP_OUTPUT_LINES = [
  '==> resolving dependencies …',
  'compile: 128 modules transformed',
  'warning: unused variable `tmp` (src/util.rs:42)',
  'linking release binary …',
  'tests: 87 passed, 0 failed, 2 skipped',
  'cache hit ratio 63% (warm cache reused)',
  'artifact packed: 24.3 MB',
]

async function runBuild(
  build: DynBuild,
  def: NonNullable<ReturnType<typeof findPipeline>>,
): Promise<void> {
  await sleep(2500) // 排队节奏：2.5s 后被调度运行
  if (build.cancelled) return
  build.status = 'running'
  build.startedAt = Date.now()

  const rng = mulberry32(build.number * 6151 + build.attempt)
  const allJobs = def.stages.flatMap((s) => s.jobs)

  for (let ji = 0; ji < allJobs.length; ji++) {
    const decl = allJobs[ji] as NonNullable<(typeof allJobs)[number]>
    const state = build.jobs[ji] as DynJob
    if (build.cancelled) return
    if (state.status !== 'queued') continue

    // 任务调度：偶发在运行中收到取消 → 由 cancelBuild 收尾。
    state.status = 'running'
    state.started_at = Date.now()
    state.agent_id = (ji % 3) + 1
    adjustAgentLoad(state.agent_id, 1)

    const isFailing = build.failPlan && ji === allJobs.length - 1
    const stepCount = decl.steps.length
    for (let si = 0; si < stepCount; si++) {
      const step = decl.steps[si] as NonNullable<(typeof decl)['steps']>[number]
      appendLog(build, decl.name, build.attempt, {
        seq: 0,
        type: 'step_start',
        step: si,
        name: step.name,
        command: step.command,
        started_at: Date.now(),
      })
      const chunks = 2 + Math.floor(rng() * 3)
      for (let c = 0; c < chunks; c++) {
        await sleep(500)
        if (build.cancelled) return
        const line = STEP_OUTPUT_LINES[(ji + si + c) % STEP_OUTPUT_LINES.length] as string
        appendLog(build, decl.name, build.attempt, {
          seq: 0,
          type: 'output',
          stream: isFailing && c === chunks - 1 ? 'stderr' : 'stdout',
          text: `[${decl.name}] ${line}\n`,
        })
      }
      const failed = isFailing && si === stepCount - 1
      appendLog(build, decl.name, build.attempt, {
        seq: 0,
        type: 'step_end',
        step: si,
        exit_code: failed ? 1 : 0,
        duration_ms: (chunks + 1) * 500,
      })
      if (failed) {
        state.status = 'failed'
        state.exit_code = 1
        state.finished_at = Date.now()
        state.detail = '步骤退出码 1：命令执行失败（mock 生命周期模拟）'
        adjustAgentLoad(state.agent_id, -1)
        appendLog(build, decl.name, build.attempt, {
          seq: 0,
          type: 'job_end',
          status: 'failed',
          exit_code: 1,
        })
        // fail-fast：后续任务跳过，构建落失败终态。
        for (const later of build.jobs) {
          if (later.status === 'queued') later.status = 'skipped'
        }
        build.status = 'failed'
        build.finishedAt = Date.now()
        return
      }
    }

    state.status = 'succeeded'
    state.exit_code = 0
    state.finished_at = Date.now()
    adjustAgentLoad(state.agent_id, -1)
    appendLog(build, decl.name, build.attempt, {
      seq: 0,
      type: 'job_end',
      status: 'succeeded',
      exit_code: 0,
    })
    // 产物上传（声明即上传，成功后立即可下载）。
    for (const up of decl.artifact_uploads ?? []) {
      build.artifacts.push({
        name: up.name,
        size: 512e3 + Math.floor(rng() * 40e6),
        sha256: Array.from({ length: 64 }, () => '0123456789abcdef'[Math.floor(rng() * 16)]).join(''),
        created_at: Date.now(),
      })
    }
    await sleep(400) // 任务间调度间隔
  }

  build.status = 'succeeded'
  build.finishedAt = Date.now()
}

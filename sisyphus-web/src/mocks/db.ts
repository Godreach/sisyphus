// MSW 契约 mock 数据源（ADR-0024，票 #101）：fixture 即开发数据也是测试数据。
//
// - 规模接近真实：11 个项目、20+ 条流水线、200+ 条覆盖全状态矩阵的构建
//   （succeeded/failed/cancelled/timeout/queued/running，含 attempt>1 与
//   cron/poll/manual 三触发源）、7 台多状态 Agent（在线/离线/停用/排空/
//   不兼容/升级中）。全量确定性生成（种子随机），模块加载时构建一次。
// - 空态/错误态钩子：`empty-repo` 项目两条流水线零构建（空列表）；
//   `docs-site/release` 同理；`error-demo` 项目的端点一律 500（handlers.ts
//   拦截，演示整页报错/重试）；概览端点支持 `?_mock_error=1` 返回 500。
// - 动态构建（触发/重跑）由 `engine.ts` 管理，与 fixture 在 handlers 层合并。

import type {
  AgentResponse,
  ArtifactResponse,
  BuildStatusDto,
  BuildSummaryResponse,
  JobStatusDto,
  ModelParameterDecl,
  OverviewSnapshotResponse,
  ProjectResponse,
  RecentBuildDto,
  TriggerSourceDto,
  VersionDto,
} from '@/api/types'

/** 种子随机（mulberry32）：全量 fixture 确定性可复现。 */
export function mulberry32(seed: number): () => number {
  let a = seed >>> 0
  return () => {
    a |= 0
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

/** fixture 时间基线（模块加载时刻；列表按相对时间展示，无需绝对对齐）。 */
const NOW = Date.now()

// ---------------------------------------------------------------------------
// 用户（登录 mock：admin/admin123、alice/alice123、bob/bob123）
// ---------------------------------------------------------------------------

export interface FixtureUser {
  username: string
  password: string
  is_admin: boolean
}

export const USERS: FixtureUser[] = [
  { username: 'admin', password: 'admin123', is_admin: true },
  { username: 'alice', password: 'alice123', is_admin: false },
  { username: 'bob', password: 'bob123', is_admin: false },
]

// ---------------------------------------------------------------------------
// 项目
// ---------------------------------------------------------------------------

function project(id: number, name: string, url: string): ProjectResponse {
  return {
    id,
    name,
    scm_type: 'git',
    scm_url: url,
    default_branch: 'main',
    created_at: NOW - 90 * 86400e3,
    updated_at: NOW - 3 * 86400e3,
  }
}

export const PROJECTS: ProjectResponse[] = [
  project(1, 'web-app', 'https://github.com/acme/web-app.git'),
  project(2, 'api-gateway', 'https://github.com/acme/api-gateway.git'),
  project(3, 'cli-tool', 'https://github.com/acme/cli-tool.git'),
  project(4, 'mobile-app', 'https://github.com/acme/mobile-app.git'),
  project(5, 'infra-terraform', 'https://github.com/acme/infra-terraform.git'),
  project(6, 'sdk-python', 'https://github.com/acme/sdk-python.git'),
  project(7, 'data-pipeline', 'https://github.com/acme/data-pipeline.git'),
  project(8, 'docs-site', 'https://github.com/acme/docs-site.git'),
  project(9, 'android-sdk', 'https://github.com/acme/android-sdk.git'),
  project(10, 'empty-repo', 'https://github.com/acme/empty-repo.git'),
  // 错误态演示项目：其全部端点在 handlers 层固定返回 500（见 handlers.ts）。
  project(11, 'error-demo', 'https://github.com/acme/error-demo.git'),
]

// ---------------------------------------------------------------------------
// 流水线定义（model Pipeline JSON 形态的 fixture：参数 + 阶段/任务声明）
// ---------------------------------------------------------------------------

export interface FixtureStepDecl {
  /** 步骤名（SSE step_start 事件回显用；空串 = 未命名 shell 步骤）。 */
  name: string
  /** 命令行（Agent 始终回显进日志，ADR-0013）。 */
  command: string
}

export interface FixtureJobDecl {
  name: string
  labels?: string[]
  allow_failure?: boolean
  artifact_uploads?: { name: string; path: string }[]
  steps: FixtureStepDecl[]
}

export interface FixtureStageDecl {
  name: string
  jobs: FixtureJobDecl[]
}

export interface FixturePipeline {
  project: string
  name: string
  parameters: ModelParameterDecl[]
  stages: FixtureStageDecl[]
}

function stepsOf(job: string): FixtureStepDecl[] {
  return [
    { name: 'checkout', command: 'git clone --depth 1 $SISYPHUS_REPO . && git checkout $SISYPHUS_COMMIT' },
    { name: job, command: `make ${job}` },
    { name: 'cleanup', command: 'rm -rf .cache' },
  ]
}

/** 标准 main 流水线：build（compile/unit-test）→ check（lint + audit 豁免）。 */
function mainPipeline(project: string, parameters: ModelParameterDecl[] = []): FixturePipeline {
  return {
    project,
    name: 'main',
    parameters,
    stages: [
      {
        name: 'build',
        jobs: [
          { name: 'compile', labels: ['linux'], steps: stepsOf('compile') },
          { name: 'unit-test', labels: ['linux'], steps: stepsOf('unit-test') },
        ],
      },
      {
        name: 'check',
        jobs: [
          { name: 'lint', labels: ['linux'], steps: stepsOf('lint') },
          { name: 'audit', labels: ['linux'], allow_failure: true, steps: stepsOf('audit') },
        ],
      },
    ],
  }
}

/** 标准 release 流水线：参数化 + 产物上传（详情页产物区/触发对话框消费）。 */
function releasePipeline(project: string): FixturePipeline {
  return {
    project,
    name: 'release',
    parameters: [
      { name: 'version', type: 'string', required: true },
      { name: 'channel', type: 'enum', choices: ['stable', 'beta'], default: 'stable' },
      { name: 'dry_run', type: 'bool', default: false },
    ],
    stages: [
      { name: 'build', jobs: [{ name: 'compile', labels: ['linux'], steps: stepsOf('compile') }] },
      {
        name: 'package',
        jobs: [
          {
            name: 'package',
            labels: ['linux'],
            artifact_uploads: [
              { name: 'app-linux-amd64.tar.gz', path: 'dist/app-linux-amd64.tar.gz' },
              { name: 'checksums.txt', path: 'dist/checksums.txt' },
            ],
            steps: stepsOf('package'),
          },
        ],
      },
      {
        name: 'verify',
        jobs: [
          { name: 'smoke', labels: ['linux'], steps: stepsOf('smoke') },
          { name: 'sign', labels: ['linux'], steps: stepsOf('sign') },
        ],
      },
    ],
  }
}

function extraPipeline(project: string, name: string, jobs: string[]): FixturePipeline {
  return {
    project,
    name,
    parameters: [],
    stages: [{ name: 'main', jobs: jobs.map((j) => ({ name: j, labels: ['linux'], steps: stepsOf(j) })) }],
  }
}

export const PIPELINES: FixturePipeline[] = [
  mainPipeline('web-app', [{ name: 'deploy_target', type: 'enum', choices: ['staging', 'prod'], default: 'staging' }]),
  releasePipeline('web-app'),
  extraPipeline('web-app', 'nightly', ['e2e-chrome', 'e2e-firefox']),
  mainPipeline('api-gateway'),
  releasePipeline('api-gateway'),
  extraPipeline('api-gateway', 'integration', ['contract-test', 'load-test']),
  mainPipeline('cli-tool'),
  releasePipeline('cli-tool'),
  extraPipeline('cli-tool', 'docs', ['build-docs']),
  mainPipeline('mobile-app'),
  releasePipeline('mobile-app'),
  mainPipeline('infra-terraform'),
  releasePipeline('infra-terraform'),
  mainPipeline('sdk-python'),
  releasePipeline('sdk-python'),
  mainPipeline('data-pipeline'),
  releasePipeline('data-pipeline'),
  mainPipeline('docs-site'),
  releasePipeline('docs-site'),
  mainPipeline('android-sdk'),
  releasePipeline('android-sdk'),
  mainPipeline('empty-repo'),
  releasePipeline('empty-repo'),
]

/** android-sdk/main 首任务要求 gpu 标签——无 Agent 满足，且其最新一条构建
 *  强制为排队（排队原因 missing_labels 的演示源，概览 has_no_match 警示态）。 */
const GPU_JOB_PROJECT = 'android-sdk'

export function findPipeline(project: string, pipeline: string): FixturePipeline | null {
  return PIPELINES.find((p) => p.project === project && p.name === pipeline) ?? null
}

// ---------------------------------------------------------------------------
// 构建 fixture：per-pipeline 概要列表（真实规模 + 全状态矩阵），详情/产物
// 按需从概要 + 定义派生（确定性 hash，多次请求结果一致）。
// ---------------------------------------------------------------------------

/** per-pipeline 构建条数（main 多、release 次之、extras 少）。 */
function buildCountFor(pipeline: string): number {
  if (pipeline === 'main') return 13
  if (pipeline === 'release') return 8
  return 5
}

/** 最新一条构建的状态轮换：保证运行中/排队/成功/失败都有「最新」样例。 */
const NEWEST_STATUS_CYCLE: BuildStatusDto[] = ['running', 'queued', 'succeeded', 'failed']
const SECOND_STATUS_CYCLE: BuildStatusDto[] = ['succeeded', 'failed', 'cancelled', 'succeeded']

function randomStatus(rng: () => number): BuildStatusDto {
  const r = rng()
  if (r < 0.58) return 'succeeded'
  if (r < 0.76) return 'failed'
  if (r < 0.86) return 'cancelled'
  if (r < 0.92) return 'timeout'
  if (r < 0.96) return 'running'
  return 'queued'
}

function randomTrigger(rng: () => number): TriggerSourceDto {
  const r = rng()
  if (r < 0.6) return 'manual'
  if (r < 0.85) return 'cron'
  return 'poll'
}

const TRIGGER_ACTORS = ['admin', 'bob', 'alice']

interface BuildRecord {
  key: string
  summary: BuildSummaryResponse
}

const BUILD_RECORDS = new Map<string, BuildRecord[]>()
const ALL_RECORDS: BuildRecord[] = []
let nextBuildId = 1

// 模块加载时全量生成（确定性；多测试/多请求共享同一份）。
// 空态演示：这些 (project, pipeline) 对保持零构建（构建列表空态）。
const EMPTY_PIPELINES = new Set(['empty-repo/main', 'empty-repo/release', 'docs-site/release'])

{
  let seed = 20260829
  for (const pl of PIPELINES) {
    const key = `${pl.project}/${pl.name}`
    // 32 位累积推进（Math.imul 防浮点溢出——普通乘法超 2^53 后低位坍缩，
    // 状态轮换会退化成单一值）。
    seed = (Math.imul(seed, 31) + 7) >>> 0
    const rng = mulberry32(seed)
    const count = EMPTY_PIPELINES.has(key) ? 0 : buildCountFor(pl.name)
    const records: BuildRecord[] = []
    for (let i = 0; i < count; i++) {
      const number = count - i // 按号倒序生成（i=0 最新）
      let status: BuildStatusDto
      if (i === 0 && pl.project === GPU_JOB_PROJECT && pl.name === 'main') {
        status = 'queued' // 强制演示：缺 gpu 标签的排队任务（has_no_match）
      } else if (i === 0) status = NEWEST_STATUS_CYCLE[seed % NEWEST_STATUS_CYCLE.length] as BuildStatusDto
      else if (i === 1) status = SECOND_STATUS_CYCLE[(seed + 1) % SECOND_STATUS_CYCLE.length] as BuildStatusDto
      else status = randomStatus(rng)

      const trigger = randomTrigger(rng)
      const triggerBy = trigger === 'manual' ? (TRIGGER_ACTORS[seed % TRIGGER_ACTORS.length] as string) : 'admin'
      const attempt = status === 'failed' && rng() < 0.3 ? 2 : 1

      // 时间线：最新构建最近；排队未开始；运行中无结束。
      const startedAt =
        status === 'queued' ? null : NOW - (i + 1) * 3600e3 - Math.floor(rng() * 1800e3)
      const durationMs = 60e3 + Math.floor(rng() * 20 * 60e3)
      const finishedAt =
        status === 'succeeded' || status === 'failed' || status === 'timeout'
          ? (startedAt ?? NOW) + durationMs
          : status === 'cancelled'
            ? (startedAt ?? NOW) + Math.floor(durationMs / 2)
            : null
      const cancelledAt = status === 'cancelled' ? finishedAt : null

      const summary: BuildSummaryResponse = {
        number,
        pipeline_name: pl.name,
        status,
        trigger,
        trigger_by: triggerBy,
        attempt,
        started_at: startedAt,
        finished_at: finishedAt,
        cancelled_at: cancelledAt,
      }
      const record: BuildRecord = { key, summary }
      records.push(record)
      ALL_RECORDS.push(record)
      nextBuildId = Math.max(nextBuildId, number)
    }
    BUILD_RECORDS.set(key, records)
  }
  nextBuildId += 1
}

export function buildSummaries(project: string, pipeline: string): BuildSummaryResponse[] {
  return (BUILD_RECORDS.get(`${project}/${pipeline}`) ?? [])
    .filter((r) => !DELETED_BUILDS.has(buildRef(r.key, r.summary.number)))
    .map((r) => r.summary)
}

/** fixture 构建删除（手动删构建，ADR-0013）：记录保留不可真删，改为从
 *  mock 可见面摘除（列表/详情/产物一律 404）。 */
const DELETED_BUILDS = new Set<string>()

function buildRef(project: string, number: number): string {
  return `${project}#${number}`
}

/** 删 fixture 构建：返回是否存在（存在与否决定 handler 204/404）。 */
export function deleteBuildRecord(project: string, pipeline: string, number: number): boolean {
  const key = `${project}/${pipeline}`
  const record = BUILD_RECORDS.get(key)?.find((r) => r.summary.number === number)
  if (record == null) return false
  DELETED_BUILDS.add(buildRef(key, number))
  return true
}

/** 取消 fixture 构建（queued/running → cancelled 终态迁移，与真后端语义
 *  一致）；终态幂等返回原状态。返回 null = 构建不存在/已删。 */
export function cancelFixtureBuild(
  project: string,
  pipeline: string,
  number: number,
): BuildSummaryResponse | null {
  const key = `${project}/${pipeline}`
  const summary = BUILD_RECORDS.get(key)?.find((r) => r.summary.number === number)?.summary
  if (summary == null || DELETED_BUILDS.has(buildRef(key, number))) return null
  if (summary.status === 'queued' || summary.status === 'running') {
    summary.status = 'cancelled'
    summary.cancelled_at = NOW
    summary.finished_at = NOW
    if (summary.started_at == null) summary.started_at = NOW
  }
  return summary
}

export function buildSummaryAt(project: string, pipeline: string, number: number): BuildSummaryResponse | null {
  const key = `${project}/${pipeline}`
  if (DELETED_BUILDS.has(buildRef(key, number))) return null
  return BUILD_RECORDS.get(key)?.find((r) => r.summary.number === number)?.summary ?? null
}

/** 构建详情（fixture 派生）：按构建状态把定义的任务序列摊成状态计划。 */
export function buildDetailOf(project: string, pipeline: string, number: number) {
  const summary = buildSummaryAt(project, pipeline, number)
  const def = findPipeline(project, pipeline)
  if (summary == null || def == null) return null

  const rng = mulberry32(number * 7919 + pipeline.length * 131)
  const flat = def.stages.flatMap((stage, si) => stage.jobs.map((job) => ({ stage, si, job })))
  const total = flat.length

  /** 一次 attempt 的状态计划：给定终态与失败点，产出每任务状态。 */
  function plan(attemptStatus: BuildStatusDto): { status: JobStatusDto; detail: string | null }[] {
    const out: { status: JobStatusDto; detail: string | null }[] = []
    const failIdx = Math.floor(rng() * total)
    for (let i = 0; i < total; i++) {
      if (attemptStatus === 'succeeded') out.push({ status: 'succeeded', detail: null })
      else if (attemptStatus === 'queued') out.push({ status: 'queued', detail: null })
      else if (attemptStatus === 'running') {
        // 至少一个已完成 + 一个运行中（total===1 时直接运行中）。
        const cur = Math.max(1, Math.floor(rng() * total))
        if (i < cur) out.push({ status: 'succeeded', detail: null })
        else if (i === cur) out.push({ status: 'running', detail: null })
        else out.push({ status: 'queued', detail: null })
      } else if (attemptStatus === 'failed') {
        if (i < failIdx) out.push({ status: 'succeeded', detail: null })
        else if (i === failIdx) out.push({ status: 'failed', detail: '步骤退出码 1：命令执行失败（fixture 模拟）' })
        else out.push({ status: 'skipped', detail: null })
      } else if (attemptStatus === 'cancelled') {
        const cur = Math.floor(rng() * total)
        if (i < cur) out.push({ status: 'succeeded', detail: null })
        else if (i === cur) out.push({ status: 'cancelled', detail: '手动取消（fixture 模拟）' })
        else out.push({ status: 'skipped', detail: null })
      } else {
        const cur = Math.floor(rng() * total)
        if (i < cur) out.push({ status: 'succeeded', detail: null })
        else if (i === cur) out.push({ status: 'timeout', detail: '超过任务超时上限，已终止（fixture 模拟）' })
        else out.push({ status: 'skipped', detail: null })
      }
    }
    return out
  }

  // attempt 历史：attempt>1 时前面次数以失败计划并列（from_failed 重跑语义）。
  const attemptPlans: { attempt: number; plan: { status: JobStatusDto; detail: string | null }[] }[] = []
  for (let a = 1; a <= summary.attempt; a++) {
    const last = a === summary.attempt
    const attemptStatus = last ? summary.status : 'failed'
    attemptPlans.push({ attempt: a, plan: plan(attemptStatus) })
  }

  const stages = def.stages.map((stage, si) => {
    const jobs = []
    let idxInStage = 0
    for (const { job } of flat.filter((f) => f.si === si)) {
      for (const { attempt, plan: p } of attemptPlans) {
        const cell = p[flat.findIndex((f) => f.si === si && f.job.name === job.name)] as
          | { status: JobStatusDto; detail: string | null }
          | undefined
        const live = cell?.status === 'running' || cell?.status === 'queued'
        const done = cell?.status === 'succeeded' || cell?.status === 'failed' || cell?.status === 'timeout' || cell?.status === 'cancelled'
        const startedAt =
          cell?.status === 'queued' ? null : (summary.started_at ?? NOW - 60e3) + idxInStage * 30e3 + (attempt - 1) * 1200e3
        const finishedAt = done ? (startedAt as number) + 25e3 + idxInStage * 10e3 : null
        jobs.push({
          name: job.name,
          status: (cell?.status ?? 'unknown') as JobStatusDto,
          attempt,
          started_at: startedAt,
          finished_at: finishedAt,
          exit_code: cell?.status === 'failed' ? 1 : done && cell?.status !== 'cancelled' ? 0 : null,
          allow_failure: job.allow_failure ?? false,
          detail: cell?.detail ?? null,
          agent_id: cell?.status === 'queued' ? null : ((number + idxInStage) % 3) + 1,
        })
        if (live) break // 进行中任务尚无后续 attempt
      }
      idxInStage++
    }
    return { index: si, name: stage.name, jobs }
  })

  const elapsed =
    summary.finished_at != null && summary.started_at != null
      ? summary.finished_at - summary.started_at
      : summary.started_at != null
        ? NOW - summary.started_at
        : null

  return {
    number: summary.number,
    pipeline_name: pipeline,
    status: summary.status,
    trigger: summary.trigger,
    trigger_by: summary.trigger_by,
    attempt: summary.attempt,
    started_at: summary.started_at,
    finished_at: summary.finished_at,
    cancelled_at: summary.cancelled_at,
    elapsed_ms: elapsed,
    stages,
  }
}

/** 构建产物（fixture 派生）：成功构建给全部声明产物；失败给已完成任务的。 */
export function artifactsOf(project: string, pipeline: string, number: number): ArtifactResponse[] {
  const detail = buildDetailOf(project, pipeline, number)
  const def = findPipeline(project, pipeline)
  if (detail == null || def == null) return []
  const doneJobs = new Set(
    detail.stages
      .flatMap((s) => s.jobs)
      .filter((j) => j.attempt === detail.attempt && (j.status === 'succeeded' || j.status === 'failed'))
      .map((j) => j.name),
  )
  const rng = mulberry32(number * 104729 + 17)
  const items: ArtifactResponse[] = []
  for (const stage of def.stages) {
    for (const job of stage.jobs) {
      if (!doneJobs.has(job.name)) continue
      for (const up of job.artifact_uploads ?? []) {
        const size = 512e3 + Math.floor(rng() * 40e6)
        items.push({
          name: up.name,
          size,
          sha256: Array.from({ length: 64 }, () => '0123456789abcdef'[Math.floor(rng() * 16)]).join(''),
          created_at: detail.finished_at ?? NOW,
        })
      }
    }
  }
  return items
}

// ---------------------------------------------------------------------------
// Agent fixture（多状态：在线/离线/停用/排空升级中/版本不兼容）
// ---------------------------------------------------------------------------

const AGENT_VERSION_TARGET: VersionDto = { major: 1, minor: 5, patch: 0 }

function version(major: number, minor: number, patch: number): VersionDto {
  return { major, minor, patch }
}

function disk(usedGb: number, cacheMb: number, workspaceMb: number) {
  return {
    volumes: [{ mount_point: '/', total_bytes: 500e9, free_bytes: (500 - usedGb) * 1e9 }],
    cache_bytes: cacheMb * 1e6,
    workspace_bytes: workspaceMb * 1e6,
  }
}

export const AGENTS: AgentResponse[] = [
  {
    name: 'build-01',
    online: true,
    disabled: false,
    system_labels: ['linux', 'docker'],
    custom_labels: ['rust'],
    max_concurrency: 4,
    active_jobs: 1,
    last_seen_at: NOW - 15e3,
    disk_usage: disk(120, 2400, 1800),
    agent_version: AGENT_VERSION_TARGET,
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 60 * 86400e3,
    updated_at: NOW - 15e3,
  },
  {
    name: 'build-02',
    online: true,
    disabled: false,
    system_labels: ['linux'],
    custom_labels: [],
    max_concurrency: 2,
    active_jobs: 0,
    last_seen_at: NOW - 22e3,
    disk_usage: disk(80, 900, 300),
    agent_version: version(1, 4, 2),
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 55 * 86400e3,
    updated_at: NOW - 22e3,
  },
  {
    name: 'build-03',
    online: true,
    disabled: false,
    system_labels: ['windows', 'docker'],
    custom_labels: [],
    max_concurrency: 4,
    active_jobs: 4,
    last_seen_at: NOW - 9e3,
    disk_usage: disk(300, 5200, 4100),
    agent_version: AGENT_VERSION_TARGET,
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 40 * 86400e3,
    updated_at: NOW - 9e3,
  },
  {
    name: 'build-04',
    online: false,
    disabled: false,
    system_labels: ['linux', 'docker'],
    custom_labels: [],
    max_concurrency: 4,
    active_jobs: 0,
    last_seen_at: NOW - 2 * 3600e3,
    disk_usage: disk(95, 1100, 640),
    agent_version: version(1, 4, 0),
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 50 * 86400e3,
    updated_at: NOW - 2 * 3600e3,
  },
  {
    name: 'build-05',
    online: false,
    disabled: true,
    system_labels: ['macos'],
    custom_labels: [],
    max_concurrency: 2,
    active_jobs: 0,
    last_seen_at: NOW - 12 * 86400e3,
    disk_usage: disk(60, 100, 90),
    agent_version: version(1, 3, 0),
    version_compatible: true,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 70 * 86400e3,
    updated_at: NOW - 12 * 86400e3,
  },
  {
    name: 'build-06',
    online: true,
    disabled: false,
    system_labels: ['linux'],
    custom_labels: [],
    max_concurrency: 2,
    active_jobs: 1,
    last_seen_at: NOW - 11e3,
    disk_usage: disk(140, 700, 520),
    agent_version: version(1, 4, 1),
    version_compatible: true,
    draining: true,
    upgrade_phase: 'downloading',
    upgrade_error: null,
    created_at: NOW - 30 * 86400e3,
    updated_at: NOW - 5 * 60e3,
  },
  {
    name: 'build-07',
    online: true,
    disabled: false,
    system_labels: ['linux'],
    custom_labels: [],
    max_concurrency: 4,
    active_jobs: 0,
    last_seen_at: NOW - 30e3,
    disk_usage: disk(210, 3300, 2900),
    agent_version: version(0, 9, 5),
    version_compatible: false,
    draining: false,
    upgrade_phase: null,
    upgrade_error: null,
    created_at: NOW - 80 * 86400e3,
    updated_at: NOW - 30e3,
  },
]

// ---------------------------------------------------------------------------
// 概览快照（从 fixture 派生：stat 卡全量真值 + 三类事实警示 + 最近构建）
// ---------------------------------------------------------------------------

export function overviewSnapshot(): OverviewSnapshotResponse {
  const online = AGENTS.filter((a) => a.online && !a.disabled)
  const slotsTotal = online.reduce((sum, a) => sum + a.max_concurrency, 0)
  const slotsUsed = online.reduce((sum, a) => sum + a.active_jobs, 0)

  const terminal = { succeeded: 0, failed: 0, cancelled: 0, timeout: 0 }
  let queueDepth = 0
  for (const r of ALL_RECORDS) {
    if (DELETED_BUILDS.has(buildRef(r.key, r.summary.number))) continue
    const s = r.summary.status
    if (s === 'succeeded' || s === 'failed' || s === 'cancelled' || s === 'timeout') {
      terminal[s] += 1
    }
    if (s === 'queued') queueDepth += 1
  }

  // 队列：排队构建逐条归因（gpu 任务 → missing_labels；其余 no_slot）。
  const queueReasons = new Map<string, number>()
  for (const r of ALL_RECORDS) {
    if (r.summary.status !== 'queued' || DELETED_BUILDS.has(buildRef(r.key, r.summary.number))) continue
    const def = findPipeline(r.key.split('/')[0] as string, r.key.split('/')[1] as string)
    const firstLabels = def?.stages[0]?.jobs[0]?.labels ?? []
    const missing = firstLabels.some((l) => !AGENTS.some((a) => a.system_labels.includes(l) || a.custom_labels.includes(l)))
    const reason = missing ? 'missing_labels' : 'no_slot'
    queueReasons.set(reason, (queueReasons.get(reason) ?? 0) + 1)
  }

  const recent: RecentBuildDto[] = [...ALL_RECORDS]
    .filter((r) => !DELETED_BUILDS.has(buildRef(r.key, r.summary.number)))
    .sort((a, b) => (b.summary.finished_at ?? b.summary.started_at ?? 0) - (a.summary.finished_at ?? a.summary.started_at ?? 0))
    .slice(0, 12)
    .map((r) => ({
      project: r.key.split('/')[0] as string,
      pipeline: r.key.split('/')[1] as string,
      number: r.summary.number,
      status: r.summary.status,
      trigger: r.summary.trigger,
      started_at: r.summary.started_at,
      finished_at: r.summary.finished_at,
    }))

  return {
    queue_depth: queueDepth,
    queue_reasons: [...queueReasons.entries()].map(([reason, depth]) => ({ reason, depth })),
    agents_online: online.length,
    agents_total: AGENTS.length,
    slots_used: slotsUsed,
    slots_total: slotsTotal,
    builds_terminal: terminal,
    artifact_bytes: 3_214_890_000,
    log_bytes: 1_872_300_000,
    alerts: {
      has_no_match: queueReasons.has('missing_labels'),
      has_offline_agent: AGENTS.some((a) => !a.disabled && !a.online),
      has_draining_incompatible: AGENTS.some((a) => a.draining || !a.version_compatible),
    },
    recent_builds: recent,
  }
}

/** 触发新的 fixture 外动态构建时的起始号（per-pipeline 递增，engine.ts 消费）。 */
export function nextBuildNumber(project: string, pipeline: string): number {
  const maxFixture = Math.max(0, ...buildSummaries(project, pipeline).map((s) => s.number))
  return maxFixture + 1
}

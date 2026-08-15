// PROTOTYPE - throwaway (ticket #15). Fictional mock data covering the
// domain concepts from CONTEXT.md / ADRs.

export type BuildStatus = 'queued' | 'running' | 'success' | 'failure' | 'cancelled' | 'unknown' | 'skipped'

export interface Step {
  id: string
  type: 'shell' | 'checkout'
  name: string
  command?: string
  submodules?: boolean
}

export interface Job {
  id: string
  name: string
  labels: string[]
  containerImage?: string
  when?: string
  env: Record<string, string>
  secrets: string[]
  steps: Step[]
  allowFailure?: boolean
  retry?: number
  timeoutMin?: number
  artifacts?: string[]
  caches?: { key: string; paths: string[] }[]
  status: BuildStatus
  agentName?: string
  durationSec?: number
  missingLabels?: string[]
}

export interface Stage {
  id: string
  name: string
  when?: string
  jobs: Job[]
}

export interface Pipeline {
  id: string
  name: string
  rev: number
  params: { name: string; type: 'string' | 'number' | 'bool' | 'enum'; default: string; required: boolean }[]
  stages: Stage[]
}

export interface Project {
  id: string
  name: string
  scm: 'git' | 'svn'
  repoUrl: string
  defaultBranch?: string
  revision?: string
  pipelines: Pipeline[]
}

export const projects: Project[] = [
  {
    id: 'p1',
    name: 'sisyphus',
    scm: 'git',
    repoUrl: 'https://github.com/Godreach/sisyphus.git',
    defaultBranch: 'main',
    revision: 'r14',
    pipelines: [
      {
        id: 'pl1',
        name: 'main-ci',
        rev: 14,
        params: [
          { name: 'profile', type: 'enum', default: 'debug', required: true },
          { name: 'run_bench', type: 'bool', default: 'false', required: true },
        ],
        stages: [
          {
            id: 's1',
            name: '构建',
            jobs: [
              {
                id: 'j1',
                name: 'build-linux',
                labels: ['linux', 'x86_64'],
                env: { CARGO_TERM_COLOR: 'always' },
                secrets: [],
                containerImage: 'rust:1.83',
                caches: [{ key: 'cargo-${SISY_PIPELINE}', paths: ['target/debug/incremental'] }],
                steps: [
                  { id: 'st1', type: 'checkout', name: 'checkout', submodules: true },
                  { id: 'st2', type: 'shell', name: 'cargo build', command: 'cargo build --profile ${profile}' },
                ],
                artifacts: ['target/debug/sisyphus'],
                status: 'success',
                agentName: 'build-linux-01',
                durationSec: 342,
              },
              {
                id: 'j2',
                name: 'build-windows',
                labels: ['windows', 'x86_64'],
                env: {},
                secrets: [],
                steps: [
                  { id: 'st3', type: 'checkout', name: 'checkout' },
                  { id: 'st4', type: 'shell', name: 'cargo build', command: 'cargo build' },
                ],
                status: 'running',
                agentName: 'win-runner-02',
              },
            ],
          },
          {
            id: 's2',
            name: '测试',
            when: 'always()',
            jobs: [
              {
                id: 'j3',
                name: 'test',
                labels: ['linux'],
                env: {},
                secrets: ['NPM_TOKEN'],
                steps: [
                  { id: 'st5', type: 'checkout', name: 'checkout' },
                  { id: 'st6', type: 'shell', name: 'cargo test', command: 'cargo test --workspace' },
                ],
                retry: 1,
                status: 'queued',
                missingLabels: ['linux'],
              },
              {
                id: 'j4',
                name: 'lint',
                labels: ['linux'],
                env: {},
                secrets: [],
                steps: [{ id: 'st7', type: 'shell', name: 'clippy', command: 'cargo clippy -- -D warnings' }],
                allowFailure: true,
                status: 'skipped',
              },
            ],
          },
          {
            id: 's3',
            name: '打包',
            jobs: [
              {
                id: 'j5',
                name: 'package',
                labels: ['linux', 'x86_64'],
                env: {},
                secrets: [],
                steps: [
                  { id: 'st8', type: 'shell', name: 'tar', command: 'tar czf sisyphus.tar.gz sisyphus' },
                ],
                artifacts: ['sisyphus.tar.gz', 'sha256.txt'],
                status: 'queued',
              },
            ],
          },
        ],
      },
    ],
  },
  {
    id: 'p2',
    name: 'legacy-svn-app',
    scm: 'svn',
    repoUrl: 'svn://svn.internal.example/repo/app',
    revision: 'r231',
    pipelines: [
      {
        id: 'pl2',
        name: 'nightly',
        rev: 23,
        params: [],
        stages: [
          {
            id: 's4',
            name: '构建',
            jobs: [
              {
                id: 'j6',
                name: 'make',
                labels: ['linux'],
                env: {},
                secrets: [],
                steps: [{ id: 'st9', type: 'checkout', name: 'checkout @ HEAD' }],
                status: 'failure',
                agentName: 'build-linux-02',
                durationSec: 88,
              },
            ],
          },
        ],
      },
    ],
  },
]

export interface Build {
  id: string
  projectId: string
  pipelineId: string
  number: number
  attempt: number
  status: BuildStatus
  triggeredBy: string
  triggerKind: 'manual' | 'cron' | 'poll'
  commit: string
  startedAt: string
  durationSec: number
  stages: Stage[]
}

export const builds: Build[] = [
  {
    id: 'b1',
    projectId: 'p1',
    pipelineId: 'pl1',
    number: 128,
    attempt: 1,
    status: 'running',
    triggeredBy: 'tanweijian',
    triggerKind: 'manual',
    commit: 'f443740',
    startedAt: '2026-08-15 10:32:07',
    durationSec: 420,
    stages: projects[0].pipelines[0].stages,
  },
  {
    id: 'b2',
    projectId: 'p2',
    pipelineId: 'pl2',
    number: 64,
    attempt: 2,
    status: 'failure',
    triggeredBy: 'cron nightly',
    triggerKind: 'cron',
    commit: 'r231',
    startedAt: '2026-08-15 03:00:00',
    durationSec: 88,
    stages: projects[1].pipelines[0].stages,
  },
  {
    id: 'b3',
    projectId: 'p1',
    pipelineId: 'pl1',
    number: 127,
    attempt: 1,
    status: 'success',
    triggeredBy: 'poll-scm',
    triggerKind: 'poll',
    commit: '98cf5cb',
    startedAt: '2026-08-14 18:11:45',
    durationSec: 510,
    stages: projects[0].pipelines[0].stages,
  },
  {
    id: 'b4',
    projectId: 'p1',
    pipelineId: 'pl1',
    number: 126,
    attempt: 1,
    status: 'unknown',
    triggeredBy: 'poll-scm',
    triggerKind: 'poll',
    commit: 'c78ccee',
    startedAt: '2026-08-14 09:03:12',
    durationSec: 0,
    stages: projects[0].pipelines[0].stages,
  },
]

export type AgentState = 'online' | 'offline' | 'draining' | 'incompatible'

export interface Agent {
  id: string
  name: string
  state: AgentState
  version: string
  platform: string
  slotsUsed: number
  slotsTotal: number
  systemLabels: string[]
  customLabels: string[]
  diskTotalGb: number
  diskFreeGb: number
  cacheGb: number
  workspaceGb: number
  lastSeen: string
}

export const agents: Agent[] = [
  { id: 'a1', name: 'build-linux-01', state: 'online', version: '1.0.3', platform: 'Linux x86_64', slotsUsed: 1, slotsTotal: 2, systemLabels: ['linux', 'x86_64', 'docker'], customLabels: ['rust', 'big-disk'], diskTotalGb: 931, diskFreeGb: 118, cacheGb: 42.1, workspaceGb: 19.6, lastSeen: '3s ago' },
  { id: 'a2', name: 'build-linux-02', state: 'online', version: '1.0.3', platform: 'Linux aarch64', slotsUsed: 0, slotsTotal: 1, systemLabels: ['linux', 'aarch64', 'docker'], customLabels: [], diskTotalGb: 465, diskFreeGb: 210, cacheGb: 8.4, workspaceGb: 3.2, lastSeen: '1s ago' },
  { id: 'a3', name: 'win-runner-02', state: 'online', version: '1.0.3', platform: 'Windows x86_64', slotsUsed: 1, slotsTotal: 1, systemLabels: ['windows', 'x86_64'], customLabels: ['msvc'], diskTotalGb: 1023, diskFreeGb: 640, cacheGb: 21.7, workspaceGb: 12.4, lastSeen: 'now' },
  { id: 'a4', name: 'mac-arm-mini', state: 'offline', version: '1.0.3', platform: 'macOS aarch64', slotsUsed: 0, slotsTotal: 1, systemLabels: ['macos', 'aarch64'], customLabels: ['ios'], diskTotalGb: 994, diskFreeGb: 512, cacheGb: 30.2, workspaceGb: 8.8, lastSeen: '14m ago' },
  { id: 'a5', name: 'build-linux-03', state: 'draining', version: '1.0.2', platform: 'Linux x86_64', slotsUsed: 1, slotsTotal: 2, systemLabels: ['linux', 'x86_64', 'docker'], customLabels: ['rust'], diskTotalGb: 931, diskFreeGb: 402, cacheGb: 15.0, workspaceGb: 4.1, lastSeen: 'now' },
  { id: 'a6', name: 'old-runner', state: 'incompatible', version: '0.9.7', platform: 'Linux x86_64', slotsUsed: 0, slotsTotal: 1, systemLabels: ['linux', 'x86_64'], customLabels: [], diskTotalGb: 256, diskFreeGb: 130, cacheGb: 2.0, workspaceGb: 0.9, lastSeen: 'now' },
]

export const secrets = [
  { name: 'NPM_TOKEN', updatedAt: '2026-08-02', updatedBy: 'tanweijian', referencedBy: ['sisyphus/main-ci › test'] },
  { name: 'DOCKER_HUB_PWD', updatedAt: '2026-07-21', updatedBy: 'alice', referencedBy: ['legacy-svn-app/nightly › make'] },
]

export const auditEntries = [
  { time: '2026-08-15 10:31:02', operator: 'tanweijian', event: '登录成功', detail: '192.168.1.23' },
  { time: '2026-08-15 09:58:41', operator: 'alice', event: '机密覆写', detail: 'secrets/NPM_TOKEN (legacy-svn-app)' },
  { time: '2026-08-15 09:40:15', operator: 'tanweijian', event: 'Agent 禁用', detail: 'agents/mac-arm-mini' },
  { time: '2026-08-14 17:12:00', operator: 'tanweijian', event: '全局配置变更', detail: 'smtp.host' },
  { time: '2026-08-14 16:55:31', operator: 'bob', event: '登录失败', detail: '密码错误 ×3（限流）' },
]

export const users = [
  { name: 'tanweijian', role: 'globalAdmin', disabled: false },
  { name: 'alice', role: 'admin', disabled: false },
  { name: 'bob', role: 'runner', disabled: false },
  { name: 'ci-bot', role: 'viewer', disabled: true },
]

// Log event stream per ADR-0013: output chunks + step lifecycle interleaved, per-job seq.
export const logEvents = [
  { seq: 1, kind: 'step-start', step: 'checkout', text: '$ git clone --depth 1 https://github.com/Godreach/sisyphus.git .' },
  { seq: 2, kind: 'out', stream: 'stdout', text: "Cloning into '.'..." },
  { seq: 3, kind: 'out', stream: 'stdout', text: 'HEAD is now at f443740 docs: ADR-0019 可观测性' },
  { seq: 4, kind: 'step-end', step: 'checkout', exitCode: 0 },
  { seq: 5, kind: 'step-start', step: 'cargo build', text: '$ cargo build --profile debug' },
  { seq: 6, kind: 'out', stream: 'stdout', text: '   Compiling sisyphus-proto v1.0.0' },
  { seq: 7, kind: 'out', stream: 'stdout', text: '   Compiling sisyphus-model v1.0.0' },
  { seq: 8, kind: 'out', stream: 'stderr', text: 'warning: unused import: `std::io`' },
  { seq: 9, kind: 'out', stream: 'stdout', text: '    Finished dev [unoptimized] target(s) in 5m42s' },
  { seq: 10, kind: 'step-end', step: 'cargo build', exitCode: 0 },
]

export const agentUpgrade = {
  uploadedVersion: '1.0.3',
  uploadedAt: '2026-08-10',
  serverVersion: '1.0.3',
  compatWindow: 'N-1 (≥ 1.0.2)',
}

// REST 统一错误形态（ADR-0005，Spec B2a §4）：code / message / detail。
// 后端（sisyphus-server/src/api/error.rs）的 `ErrorBody` / `ValidationIssue`
// 的前端镜像——按 code 分支消费、校验清单 `detail.errors` 按字段路径定位。

/** 校验错误条目：字段定位路径 + 人读描述（与后端 `ValidationIssue` 同形态）。 */
export interface ValidationIssue {
  path: string
  message: string
}

/** 统一错误响应体（后端 `ErrorBody` 的镜像）。 */
export interface ErrorBody {
  /** 机器可读错误码（大写蛇形，如 `NOT_FOUND`、`VALIDATION_FAILED`）。 */
  code: string
  /** 人读错误信息，可直接展示。 */
  message: string
  /** 结构化补充（校验错误清单 `errors`、限流 `retry_after_ms` 等）。 */
  detail?: {
    errors?: ValidationIssue[]
    retry_after_ms?: number
    [key: string]: unknown
  } | null
}

/** 网络层失败（无 HTTP 响应）的统一错误码（客户端本地发出，非后端返回）。 */
export const NETWORK_ERROR_CODE = 'NETWORK_ERROR'

// ---------------------------------------------------------------------------
// 认证 / Agent / 项目 DTO（后端 `api/*.rs` 契约的前端镜像，ADR-0005）。
// 本批（B4-T2）只镜像 setup wizard / Agent 注册命令 / 登录登出消费的字段；
// 其余字段随消费页面票按需补全。
// ---------------------------------------------------------------------------

/** 项目仓库类型（后端 `ScmTypeDto`，`git` / `svn`）。 */
export type ScmTypeDto = 'git' | 'svn'

/** 项目视图（后端 `ProjectResponse`：id/name/scm_type/scm_url/default_branch）。 */
export interface ProjectResponse {
  id: number
  name: string
  scm_type: ScmTypeDto
  scm_url: string
  default_branch: string | null
  created_at: number
  updated_at: number
}

/** 建项目请求体（后端 `CreateProjectRequest`）。 */
export interface CreateProjectRequest {
  name: string
  scm_type: ScmTypeDto
  scm_url: string
  default_branch?: string | null
}

// ---------------------------------------------------------------------------
// Pipeline 定义 DTO（后端 `api/pipelines.rs` 的 `definition` 包裹体；定义本体
// 是 sisyphus-model 的 JSON 形态——前端只镜像包裹体，定义结构以 model 为
// 单一事实源，ADR-0009/0020）。
// ---------------------------------------------------------------------------

/** GET pipeline 定义响应（包裹体镜像；`definition` 即 model Pipeline JSON）。 */
export interface PipelineDefinitionResponse {
  definition: PipelineDefinitionPayload
  /** 当前修订版本号（每次保存 +1）。 */
  revision: number
  /** 最后保存的操作人。 */
  operator: string
  /** 最后保存时间（Unix 毫秒）。 */
  updated_at: number
}

/** Pipeline 定义载荷：model Pipeline JSON（本页只读消费，不校验）。 */
export type PipelineDefinitionPayload = Record<string, unknown>

/** model Pipeline 内的任务声明（构建详情产物区/等待标签态只读消费的字段）。
 *  这里只镜像本页用到的子结构，不复制整份 model 类型（ADR-0009 单一事实源
 *  由后续生成/对账管线落定）。 */
export interface ModelJobDecl {
  name: string
  labels?: string[]
  artifact_uploads?: { name: string; path: string }[]
}

/** model Pipeline 内的阶段声明（只读消费阶段序与任务）。 */
export interface ModelStageDecl {
  name: string
  jobs?: ModelJobDecl[]
}

/** model Pipeline 参数定义（触发对话框参数覆盖的表单来源）。 */
export interface ModelParameterDecl {
  name: string
  type: 'string' | 'number' | 'bool' | 'enum'
  required?: boolean
  default?: string | number | boolean
  choices?: string[]
}

/** model Pipeline（本页只读消费的字段子集）。 */
export interface ModelPipeline {
  name?: string
  parameters?: ModelParameterDecl[]
  stages?: ModelStageDecl[]
}

/** 磁盘占用（详情视图，后端 `DiskUsageDto`；本批不消费字段，仅镜像）。 */
export interface DiskUsageDto {
  volumes: { mount_point: string; total_bytes: number; free_bytes: number }[]
  cache_bytes: number
  workspace_bytes: number
}

/** Agent 管理视图（后端 `AgentResponse`，ADR-0019 字段镜像）。 */
export interface AgentResponse {
  name: string
  online: boolean
  disabled: boolean
  system_labels: string[]
  custom_labels: string[]
  max_concurrency: number
  active_jobs: number
  last_seen_at: number | null
  disk_usage?: DiskUsageDto | null
  created_at: number
  updated_at: number
}

/** 建 Agent 条目响应：token 与注册码明文仅此一次返回（后端
 *  `CreatedAgentResponse`，ADR-0010——wizard Agent 步消费后即丢弃）。 */
export interface CreatedAgentResponse {
  token: string
  register_code: string
  agent: AgentResponse
}

// ---------------------------------------------------------------------------
// 构建 / pipeline DTO（后端 `api/builds.rs`、`api/pipelines.rs`）。本批
// （B4-T3）只镜像消费到的 pipeline 定义探测形态；构建 DTO 随消费页面票
// （B4-T4 构建详情）按需补全。
// ---------------------------------------------------------------------------

/** Pipeline 定义响应（后端 `PipelineDefinitionResponse`：定义原文 + 修订）。 */
export interface PipelineDefinitionResponse {
  definition: Record<string, unknown>
  revision: number
  operator: string
  updated_at: number
}

/** 建 Agent 条目请求体（后端 `CreateAgentRequest`）。 */
export interface CreateAgentRequest {
  name: string
  custom_labels?: string[]
  max_concurrency?: number
}

/** 启停 / 编辑 Agent 请求体（后端 `PatchAgentRequest`，PATCH 语义：字段均可选，
 *  只更新出现者）。停用（`disabled: true`）即踢线；`custom_labels` 为整组替换。 */
export interface PatchAgentRequest {
  disabled?: boolean
  max_concurrency?: number
  custom_labels?: string[]
}

// ---------------------------------------------------------------------------
// 构建（builds）DTO（后端 `api/builds.rs` 契约的前端镜像，ADR-0005）。
// 票 B4-T4 构建详情/列表页消费；枚举值取 `BuildStatus::as_str()` 落库文本
// （API 层 `BuildStatusDto`/`JobStatusDto` 的 `serde(rename_all="lowercase")`
// 序列化同值）。
// ---------------------------------------------------------------------------

/** 构建状态（REST 形态，与后端 `BuildStatusDto` 同值域）。 */
export type BuildStatusDto =
  | 'queued'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'timeout'

/** 任务状态（REST 形态，与后端 `JobStatusDto` 同值域）。 */
export type JobStatusDto =
  | 'queued'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'skipped'
  | 'unknown'
  | 'timeout'
  | 'aborted'

/** 构建触发源（手动 / cron / poll）。 */
export type TriggerSourceDto = 'manual' | 'cron' | 'poll'

/** 重跑模式（`from_scratch` 新号 / `from_failed` 同号 attempt+1）。 */
export type RerunModeDto = 'from_scratch' | 'from_failed'

/** 手动触发请求体：参数覆盖 + 可选 git 分支/commit 或 svn revision。 */
export interface TriggerBuildRequest {
  /** 参数覆盖（名→值；默认值之上叠加，不存在的名无害）。 */
  params?: Record<string, string>
  /** git 分支（手动可选；缺省项目默认分支）。 */
  branch?: string | null
  /** git commit sha（手动未钉为空——Agent 检分支头）。 */
  commit?: string | null
  /** svn revision（手动可选；git 为空）。 */
  revision?: string | null
}

/** 重跑请求体：模式二选一。 */
export interface RerunBuildRequest {
  mode: RerunModeDto
}

/** 触发 / 重跑 / 取消的受理响应（202）：构建号 + 当前状态（异步推进）。 */
export interface BuildAcceptedResponse {
  /** per-pipeline 构建号。 */
  number: number
  /** 构建行 id。 */
  build_id: number
  /** 重跑 attempt（首跑 1；from_failed +1）。 */
  attempt: number
  /** 受理时状态（queued / running）。 */
  status: BuildStatusDto
}

/** 构建列表条目（概要）。 */
export interface BuildSummaryResponse {
  /** per-pipeline 构建号。 */
  number: number
  /** pipeline 名。 */
  pipeline_name: string
  /** 构建状态。 */
  status: BuildStatusDto
  /** 触发源。 */
  trigger: TriggerSourceDto
  /** 触发人（业务表实名）。 */
  trigger_by: string
  /** 重跑 attempt。 */
  attempt: number
  /** 开始时刻（queued→running；未运行为空）。 */
  started_at: number | null
  /** 终态时刻。 */
  finished_at: number | null
  /** 取消时刻。 */
  cancelled_at: number | null
}

/** 分页列表响应（按号倒序）。 */
export interface BuildListResponse {
  /** 本页构建概要。 */
  items: BuildSummaryResponse[]
  /** 总条数（满足过滤条件；供客户端算页数）。 */
  total: number
  /** 当前页码。 */
  page: number
  /** 单页条数。 */
  limit: number
}

/** 任务视图（构建详情内；含 attempt 历史——重跑后同任务多行并列）。 */
export interface JobViewDto {
  /** 任务名。 */
  name: string
  /** 任务状态。 */
  status: JobStatusDto
  /** 第几次执行（同任务重跑 attempt+1）。 */
  attempt: number
  /** 开始时刻。 */
  started_at: number | null
  /** 终态时刻。 */
  finished_at: number | null
  /** 退出码（可空）。 */
  exit_code: number | null
  /** allow_failure 豁免 fail-fast。 */
  allow_failure: boolean
  /** 详情（失败原因、缺失机密名、超时等；机密只记名不泄值）。 */
  detail: string | null
  /** 调度到的 Agent 行 id（未调度为空）。 */
  agent_id: number | null
}

/** 阶段视图（构建详情内；阶段名取自构建快照）。 */
export interface StageViewDto {
  /** 阶段序号（从 0 起）。 */
  index: number
  /** 阶段名（快照内定义）。 */
  name: string
  /** 阶段内任务（含 attempt 历史）。 */
  jobs: JobViewDto[]
}

/** 构建详情响应。 */
export interface BuildDetailResponse {
  /** per-pipeline 构建号。 */
  number: number
  /** pipeline 名。 */
  pipeline_name: string
  /** 构建状态。 */
  status: BuildStatusDto
  /** 触发源。 */
  trigger: TriggerSourceDto
  /** 触发人。 */
  trigger_by: string
  /** 重跑 attempt。 */
  attempt: number
  /** 开始时刻。 */
  started_at: number | null
  /** 终态时刻。 */
  finished_at: number | null
  /** 取消时刻。 */
  cancelled_at: number | null
  /** 耗时（毫秒；已完成 = finished-started，运行中 = now-started，未运行为空）。 */
  elapsed_ms: number | null
  /** 阶段与任务状态（按快照阶段序）。 */
  stages: StageViewDto[]
}

// 成员 / 用户目录 DTO（后端 `api/members.rs`、`api/users.rs`，ADR-0014）。
// 本批（B4-T3）供项目详情成员管理与概览/项目页消费。
// ---------------------------------------------------------------------------

/** 项目成员角色（后端 `RoleDto`：viewer / runner / admin，ADR-0014 三档）。 */
export type MemberRoleDto = 'viewer' | 'runner' | 'admin'

/** 成员分配请求项（后端 `MemberAssignment`；PUT 为整组替换语义）。 */
export interface MemberAssignment {
  username: string
  role: MemberRoleDto
}

/** 成员清单项（后端 `MemberResponse`，GET 响应 / PUT 后回读共用）。 */
export interface MemberResponse {
  user_id: number
  username: string
  role: MemberRoleDto
}

/** 用户目录项（后端 `DirectoryEntryResponse`：仅 id + 用户名，成员分配下拉）。 */
export interface DirectoryEntryResponse {
  id: number
  username: string
}

/** 前端可分支消费的 API 错误（非 2xx 统一落此形态）。 */
export class ApiError extends Error {
  /** HTTP 状态码（0 = 网络层失败，无响应）。 */
  readonly status: number
  /** 机器可读错误码（与 `ErrorBody.code` 同值）。 */
  readonly code: string
  /** 结构化补充（原样透传后端 detail）。 */
  readonly detail: ErrorBody['detail']

  constructor(
    status: number,
    code: string,
    message: string,
    detail: ErrorBody['detail'],
  ) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.detail = detail
  }

  /** 校验失败清单（`code === VALIDATION_FAILED` 时非空），按字段路径定位。 */
  get validationIssues(): ValidationIssue[] {
    return this.detail?.errors ?? []
  }

  /** 429 限流的剩余等待毫秒（`code === RATE_LIMITED` 时携带）。 */
  get retryAfterMs(): number | null {
    const ms = this.detail?.retry_after_ms
    return typeof ms === 'number' ? ms : null
  }
}

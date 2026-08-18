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

/** 建 Agent 条目请求体（后端 `CreateAgentRequest`）。 */
export interface CreateAgentRequest {
  name: string
  custom_labels?: string[]
  max_concurrency?: number
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

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

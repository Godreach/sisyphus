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
  /** 可选 SCM 用户名（与 password 一并加密落库，供 poll/测试连接探测用；B5-T3）。 */
  scm_username?: string | null
  /** 可选 SCM 密码/token（加密落库；永不上命令行/URL，任何端点不回显）。 */
  scm_password?: string | null
}

// ---------------------------------------------------------------------------
// SCM 探测 / 测试连接 / 分支枚举 / 凭据管理 DTO（后端 `api/scm.rs`，票 B5-T3，
// ADR-0016）。创建期测试连接/分支枚举为 ad-hoc（凭据经请求体递送、不落库）；
// 既有项目测试连接/凭据设置走存储凭据。
// ---------------------------------------------------------------------------

/** 创建期测试连接请求（ad-hoc 凭据不落库）。 */
export interface ScmProbeRequest {
  scm_type: ScmTypeDto
  scm_url: string
  username?: string | null
  password?: string | null
}

/** 测试连接响应：当前 head（git sha / svn revision）；空仓库为 null。 */
export interface ScmProbeResponse {
  head: string | null
}

/** 分支枚举请求（git only；创建期默认分支预填）。 */
export interface ScmBranchesRequest {
  scm_url: string
  username?: string | null
  password?: string | null
}

/** 一个分支（名 + head sha）。 */
export interface ScmBranch {
  name: string
  head: string
}

/** 分支枚举响应：分支清单 + 默认分支（远端 HEAD 指向）。 */
export interface ScmBranchesResponse {
  branches: ScmBranch[]
  default_branch: string | null
}

/** SCM 凭据设置请求（PUT 整份替换；username + password 皆空 = 清凭据）。 */
export interface ScmCredentialRequest {
  username?: string | null
  password?: string | null
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

/** PUT 保存 pipeline 定义响应（后端 `SaveDefinitionResponse`：保存成功返回新修订版本）。
 *  model 校验失败为 422 + `detail.errors` 错误清单整组透传（票 B4-T8 编辑器保存消费）。 */
export interface SaveDefinitionResponse {
  /** 本次保存后的修订版本号（首存 1、续存 +1）。 */
  revision: number
  /** 操作人（登录用户名）。 */
  operator: string
  /** 保存时间（Unix 毫秒）。 */
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

/** 语义版本号（后端 `VersionDto`，与 proto `Version` 同构）。 */
export interface VersionDto {
  major: number
  minor: number
  patch: number
}

/** Agent 管理视图（后端 `AgentResponse`，ADR-0017/0019 字段镜像）。 */
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
  /** 握手上报的 Agent 版本（从未握手为空）。 */
  agent_version?: VersionDto | null
  /** 版本是否兼容（落在 N-1 窗口内；ADR-0017 四态派生面）。 */
  version_compatible: boolean
  /** 排空/升级中（在线但不可派发；pending 或 draining/.../restarting 阶段）。 */
  draining: boolean
  /** 升级阶段（draining/downloading/swapping/restarting/fallback；无升级为空）。 */
  upgrade_phase?: string | null
  /** 升级失败原因（fallback 时记）。 */
  upgrade_error?: string | null
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
// 概览快照 DTO（后端 `api/overview.rs`，票 B5-T7，ADR-0019）：概览页单一
// 数据源——stat 卡全量真值 + 三类事实警示态 + 最近构建。任意登录角色可读。
// 后端 `OverviewResponse` 的前端镜像（双消费：同一份数也灌 `/metrics`）。
// ---------------------------------------------------------------------------

/** 队列深度原因分类条目（`reason` 值域：missing_labels / no_online_agent /
 *  no_slot / uncategorized——与后端 `snapshot::classify` 固定标签全集一一对应）。 */
export interface QueueReasonDto {
  /** 原因标签。 */
  reason: string
  /** 该原因下的等待任务数。 */
  depth: number
}

/** 构建终态计数（四态）。 */
export interface BuildsTerminalCountsDto {
  /** 成功。 */
  succeeded: number
  /** 失败。 */
  failed: number
  /** 取消。 */
  cancelled: number
  /** 超时。 */
  timeout: number
}

/** 事实型警示态（true 即有、false 即无；零阈值，ADR-0019）。 */
export interface OverviewAlertsDto {
  /** 存在无匹配 Agent 的任务。 */
  has_no_match: boolean
  /** 存在启用但离线的 Agent。 */
  has_offline_agent: boolean
  /** 存在排空或版本不兼容的 Agent。 */
  has_draining_incompatible: boolean
}

/** 最近构建条目（概览页列表）。 */
export interface RecentBuildDto {
  /** 项目名。 */
  project: string
  /** pipeline 名。 */
  pipeline: string
  /** per-pipeline 构建号。 */
  number: number
  /** 构建状态（queued/running/succeeded/failed/cancelled/timeout）。 */
  status: BuildStatusDto
  /** 触发源（manual/cron/poll）。 */
  trigger: TriggerSourceDto
  /** 开始时刻（Unix 毫秒；未运行 null）。 */
  started_at: number | null
  /** 终态时刻（Unix 毫秒）。 */
  finished_at: number | null
}

/** 概览快照响应（后端 `OverviewResponse`：stat 卡 + 警示态 + 最近构建）。 */
export interface OverviewSnapshotResponse {
  /** 队列深度（全部 queued 任务）。 */
  queue_depth: number
  /** 队列深度原因分类（保序）。 */
  queue_reasons: QueueReasonDto[]
  /** Agent 在线数。 */
  agents_online: number
  /** Agent 总数（含停用）。 */
  agents_total: number
  /** 槽位占用（在途任务）。 */
  slots_used: number
  /** 槽位总量（在线 Agent max_concurrency 之和）。 */
  slots_total: number
  /** 构建终态计数。 */
  builds_terminal: BuildsTerminalCountsDto
  /** 产物字节占用。 */
  artifact_bytes: number
  /** 日志字节占用。 */
  log_bytes: number
  /** 事实型警示态。 */
  alerts: OverviewAlertsDto
  /** 最近构建（跨可见项目，按最近活动倒序）。 */
  recent_builds: RecentBuildDto[]
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

// 产物（后端 `api/artifacts.rs`，票 #74 / B5-T2，ADR-0004/0007）。
// ---------------------------------------------------------------------------

/** 产物条目（构建产物列表）。 */
export interface ArtifactResponse {
  /** 产物名（任务级声明的上传名）。 */
  name: string
  /** 字节数。 */
  size: number
  /** SHA-256 校验和（十六进制小写）。 */
  sha256: string
  /** 上传时刻（Unix 毫秒）。 */
  created_at: number
}

/** 构建产物列表响应。 */
export interface BuildArtifactsResponse {
  /** 构建全部产物（按名排序）。 */
  items: ArtifactResponse[]
}

// ---------------------------------------------------------------------------
// 升级包 / 升级指令 / 工作区 / 缓存（后端 `api/upgrade_packages.rs`、
// `api/agents.rs`，票 #76 / B5-T4，ADR-0017/0011/0012）。升级包管理面全局
// admin；下载走 Agent token 鉴权。工作区/缓存 per-Agent 经通道往返。
// ---------------------------------------------------------------------------

/** 升级包视图（后端 `UpgradePackageResponse`）。 */
export interface UpgradePackageResponse {
  /** 包名（`sisyphus-agent-<ver>-<os>-<arch>`）。 */
  package_name: string
  /** 解析自文件名的版本。 */
  version: VersionDto
  /** 目标 OS（linux/macos/windows）。 */
  target_os: string
  /** 目标架构（x86_64/aarch64）。 */
  target_arch: string
  /** 字节数。 */
  size: number
  /** SHA-256 校验和（十六进制小写）。 */
  sha256: string
  /** 上传时刻（Unix 毫秒）。 */
  created_at: number
}

/** 升级指令请求体（全量 / 单台共用）：`{ package_name }`。 */
export interface UpgradeCommandRequest {
  /** 目标升级包名（已上传）。 */
  package_name: string
}

/** 全量升级受理摘要（202）。 */
export interface UpgradeIssuedSummary {
  /** 目标升级包名。 */
  package_name: string
  /** 已下发（在线即送 + 离线挂起）的 Agent 数。 */
  issued: number
  /** 跳过数（已在目标版本）。 */
  skipped: number
}

/** 工作区条目（per-Agent 列表，经通道往返；ADR-0011）。 */
export interface WorkspaceEntry {
  /** pipeline 名。 */
  pipeline: string
  /** 任务名。 */
  job: string
  /** 工作区绝对路径（Agent 侧）。 */
  path: string
  /** 最近使用时刻（Unix 毫秒）。 */
  last_used_at_ms: number
}

/** 工作区列表响应。 */
export interface WorkspaceListResponse {
  /** 工作区条目。 */
  entries: WorkspaceEntry[]
}

/** 工作区清理请求体（pipeline/job 皆空 = 全清）。 */
export interface WorkspaceCleanRequest {
  /** pipeline 名（空 = 全清）。 */
  pipeline?: string | null
  /** 任务名（空 = 该 pipeline 全部）。 */
  job?: string | null
}

/** 缓存条目（per-Agent 列表，经通道往返；ADR-0012）。 */
export interface CacheEntry {
  /** 缓存 key（含 files 哈希后缀）。 */
  key: string
  /** 所属 pipeline。 */
  pipeline: string
  /** 字节数。 */
  size_bytes: number
  /** 最近使用时刻（Unix 毫秒）。 */
  last_used_at_ms: number
}

/** 缓存列表响应。 */
export interface CacheListResponse {
  /** 缓存条目。 */
  entries: CacheEntry[]
}

/** 缓存删除请求体（key 空 = 全清）。 */
export interface CacheDeleteRequest {
  /** 缓存 key（空 = 全清；非空 = 跨 pipeline 匹配完整 key）。 */
  key?: string | null
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

// ---------------------------------------------------------------------------
// 管理四页 DTO（后端 `api/secrets.rs`、`api/audit.rs`、`api/users.rs`、
// `api/tokens.rs` 契约的前端镜像，ADR-0014/0015/0017）。票 B4-T6 管理四页消费。
// ---------------------------------------------------------------------------

/** 机密名清单项（后端 `SecretNameResponse`：值形态任何端点不回显，仅名）。 */
export interface SecretNameResponse {
  /** 机密名（env 键字符集：字母数字与下划线）。 */
  name: string
}

/** 建/覆写机密请求体（后端 `PutSecretRequest`：值只写不读，写入即加密落库）。 */
export interface PutSecretRequest {
  /** 机密值（永不可读回——任何端点不再回显，请谨慎覆写）。 */
  value: string
}

/** 全部审计事件类型（过滤下拉与值域对账的单点；与后端 `AuditEvent::ALL`
 *  同序）。`AuditEventDto` 由本数组派生（`as const` + `typeof`），新增事件
 *  只改这一处——避免「union 与数组并列、改一处漏另一处」的 Shotgun Surgery。 */
export const AUDIT_EVENTS = [
  'login_success',
  'login_failure',
  'logout',
  'user_created',
  'user_disabled',
  'user_enabled',
  'password_reset',
  'pat_created',
  'pat_revoked',
  'project_created',
  'member_roles_changed',
  'secret_created',
  'secret_overwritten',
  'secret_deleted',
  'agent_created',
  'agent_disabled',
  'agent_enabled',
  'agent_registered',
  'scm_credential_set',
  'upgrade_package_uploaded',
  'upgrade_package_deleted',
  'upgrade_command_issued',
] as const

/** 审计事件类型（由 `AUDIT_EVENTS` 派生；后端 `AuditEvent::as_str()` 契约值，
 *  与 store 层同源，19 种）。 */
export type AuditEventDto = (typeof AUDIT_EVENTS)[number]

/** 审计查询参数（全部可选，AND 组合；分页 limit/offset，时间倒序由后端保证）。
 *  type alias（非 interface）以获 TS 隐式索引签名，可直接作 `http.get` 的
 *  `query` 传入（与 `buildsApi.list` 的内联 query 类型同纪律）。 */
export type AuditQuery = {
  /** 时间下限（含；Unix 毫秒）。 */
  since?: number
  /** 时间上限（含；Unix 毫秒）。 */
  until?: number
  /** 操作人（用户名，精确匹配）。 */
  user?: string
  /** 项目名（精确匹配）。 */
  project?: string
  /** 事件类型（取值域见 `AUDIT_EVENTS`）。 */
  event?: AuditEventDto
  /** 单页条数（1..=200，默认 50）。 */
  limit?: number
  /** 跳过条数（默认 0）。 */
  offset?: number
}

/** 审计条目（后端 `AuditEntryResponse`：detail 为 JSON 对象，机密事件只记名）。 */
export interface AuditEntryResponse {
  /** 行 id。 */
  id: number
  /** 事件时间（Unix 毫秒）。 */
  ts: number
  /** 操作人（用户名）。 */
  actor: string
  /** 事件类型（`AuditEventDto` 契约值）。 */
  event: AuditEventDto
  /** 项目名（可空：非项目域事件）。 */
  project: string | null
  /** 结构化补充（可空：机密名 / 目标用户 / 成员角色清单等；机密只记名不泄值）。 */
  detail: Record<string, unknown> | null
}

/** 用户管理视图（后端 `UserResponse`：密码哈希永不出现，含 disabled）。 */
export interface UserResponse {
  /** 用户 id。 */
  id: number
  /** 用户名（唯一）。 */
  username: string
  /** 全局管理员。 */
  is_admin: boolean
  /** 禁用标志（禁用即踢线：session 与 PAT 同秒全删）。 */
  disabled: boolean
  /** 创建时间（Unix 毫秒）。 */
  created_at: number
  /** 最后更新时间（Unix 毫秒）。 */
  updated_at: number
}

/** 建号请求体（后端 `CreateUserRequest`：is_admin 默认 false，建号时显式设全局 admin）。 */
export interface CreateUserRequest {
  /** 用户名（1-64 位字母数字或 `_ . -`，trim 后生效）。 */
  username: string
  /** 密码（最小长度 8，无复杂度规则）。 */
  password: string
  /** 是否全局管理员（默认 false——admin 是建号时的显式选择；已有用户切换见退化标注）。 */
  is_admin?: boolean
}

/** 禁用/启用请求体（后端 `PatchUserRequest`：仅 disabled——切换已有用户 admin 的
 *  端点尚未交付，见 UsersView 退化标注）。 */
export interface PatchUserRequest {
  /** 目标状态：true = 禁用（同秒删其全部 session 与 PAT），false = 启用。 */
  disabled: boolean
}

/** 代办重置密码请求体（后端 `ResetPasswordRequest`：无需当前密码）。 */
export interface ResetPasswordRequest {
  /** 新密码（最小长度 8，无复杂度规则）。 */
  new_password: string
}

/** PAT 列表项（后端 `TokenResponse`：无值形态——名 / 创建时间 / 过期）。 */
export interface TokenResponse {
  /** 行 id（吊销端点的路径参数）。 */
  id: number
  /** 令牌名。 */
  name: string
  /** 过期时间（Unix 毫秒；`null` = 永不过期）。 */
  expires_at: number | null
  /** 创建时间（Unix 毫秒）。 */
  created_at: number
}

/** 创建 PAT 请求体（后端 `CreateTokenRequest`）。 */
export interface CreateTokenRequest {
  /** 令牌名（非空，trim 后生效）。 */
  name: string
  /** 过期时间（Unix 毫秒；`null` = 永不过期，须晚于当前时间）。 */
  expires_at?: number | null
}

/** 创建 PAT 响应（后端 `CreatedTokenResponse`：完整令牌仅此一次返回）。 */
export interface CreatedTokenResponse {
  /** 完整令牌（`sis_` 前缀 + 43 字符）。请立即保存：本响应是唯一一次出现。 */
  token: string
  /** 行 id。 */
  id: number
  /** 令牌名。 */
  name: string
  /** 过期时间（Unix 毫秒；`null` = 永不过期）。 */
  expires_at: number | null
  /** 创建时间（Unix 毫秒）。 */
  created_at: number
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

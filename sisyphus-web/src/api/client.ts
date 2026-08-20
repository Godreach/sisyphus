// 端点级 API 客户端（票 B4-T1：工程底座把认证面端点立起来，其余端点在
// 各页面票按需扩展）。消费既有 REST 契约（ADR-0005：`/api/v1/` 前缀），
// 错误形态统一由 `http.ts` 落 `ApiError`（code/message/detail，按 code 分支）。

import { http } from './http-singleton'
import type { CredentialsRequest, MeResponse } from './http'
import type {
  AgentResponse,
  AuditEntryResponse,
  AuditQuery,
  BuildAcceptedResponse,
  BuildArtifactsResponse,
  BuildDetailResponse,
  BuildListResponse,
  BuildStatusDto,
  CacheDeleteRequest,
  CacheListResponse,
  CreateAgentRequest,
  CreateProjectRequest,
  CreateTokenRequest,
  CreatedAgentResponse,
  CreatedTokenResponse,
  CreateUserRequest,
  DirectoryEntryResponse,
  MemberAssignment,
  MemberResponse,
  PatchAgentRequest,
  PatchUserRequest,
  PipelineDefinitionResponse,
  ProjectResponse,
  PutSecretRequest,
  OverviewSnapshotResponse,
  ResetPasswordRequest,
  RerunBuildRequest,
  SaveDefinitionResponse,
  ScmBranchesRequest,
  ScmBranchesResponse,
  ScmCredentialRequest,
  ScmProbeRequest,
  ScmProbeResponse,
  SecretNameResponse,
  TokenResponse,
  TriggerBuildRequest,
  UpgradeCommandRequest,
  UpgradeIssuedSummary,
  UpgradePackageResponse,
  UserResponse,
  WorkspaceCleanRequest,
  WorkspaceListResponse,
} from './types'

/** 认证端点（后端 `api/auth.rs`，ADR-0014）。 */
export const authApi = {
  /** 登录：用户名密码换会话 cookie，返回当前用户。 */
  login: (req: CredentialsRequest) =>
    http.post<MeResponse>('auth/login', { json: req }),

  /** 登出：删会话行 + 清 cookie（Bearer 通道无事可做，同 204）。 */
  logout: () => http.post<void>('auth/logout'),

  /** 会话恢复锚点：返回当前用户名与全局管理员标志（401 即未登录）。 */
  me: () => http.get<MeResponse>('auth/me'),
}

/** 初始化引导（setup wizard）：空库时创建首个全局管理员（404 = 已完成）。 */
export const setupApi = {
  setup: (req: CredentialsRequest) =>
    http.post<MeResponse>('auth/setup', { json: req }),
}

/** Agent 端点（后端 `api/agents.rs`，ADR-0010/0008；管理面全局 admin）。 */
export const agentsApi = {
  /** 建 Agent 条目：签发 per-Agent token + 一次性注册码（明文仅此一次）。 */
  create: (req: CreateAgentRequest) =>
    http.post<CreatedAgentResponse>('agents', { json: req }),

  /** Agent 清单（全局 admin；按名排序，含已停用）。概览页在线/总数与
   *  离线/不兼容警示态据此派生（ADR-0019）。 */
  list: () => http.get<AgentResponse[]>('agents'),

  /** Agent 详情（全局 admin）：在线/标签/槽位占用/磁盘占用（ADR-0019）。 */
  get: (name: string) => http.get<AgentResponse>(`agents/${encodeURIComponent(name)}`),

  /** 启停 / 编辑（全局 admin，PATCH 语义：字段可选）：停用即踢线；改槽位与
   *  自定义标签（整组替换）。返回落定后的 Agent。 */
  patch: (name: string, req: PatchAgentRequest) =>
    http.patch<AgentResponse>(`agents/${encodeURIComponent(name)}`, { json: req }),

  /** 全量升级（全局 admin）：向所有版本非目标包的未停用 Agent 下发升级指令。
   *  在线即送、离线挂起；返回受理摘要（issued/skipped，ADR-0017）。 */
  upgradeAll: (req: UpgradeCommandRequest) =>
    http.post<UpgradeIssuedSummary>('agents/upgrade', { json: req }),

  /** 单台升级（全局 admin）：强制该 Agent 升级到目标包；返回落定后的 Agent
   *  （含升级阶段）。已在目标版本 → 409。 */
  upgradeOne: (name: string, req: UpgradeCommandRequest) =>
    http.post<AgentResponse>(`agents/${encodeURIComponent(name)}/upgrade`, { json: req }),

  /** 工作区列表（全局 admin，经通道往返；ADR-0011）。Agent 离线 → 409，
   *  在线未在窗口内回响应 → 504。 */
  listWorkspace: (name: string) =>
    http.post<WorkspaceListResponse>(`agents/${encodeURIComponent(name)}/workspace/list`),

  /** 工作区清理（全局 admin，fire-and-forget；ADR-0011）。pipeline/job 皆空
   *  = 全清。Agent 离线 → 409。 */
  cleanWorkspace: (name: string, req: WorkspaceCleanRequest) =>
    http.post<void>(`agents/${encodeURIComponent(name)}/workspace/clean`, { json: req }),

  /** 缓存列表（全局 admin，经通道往返；ADR-0012）。 */
  listCache: (name: string) =>
    http.post<CacheListResponse>(`agents/${encodeURIComponent(name)}/cache/list`),

  /** 缓存删除（全局 admin，fire-and-forget；ADR-0012）。key 空 = 全清。 */
  deleteCache: (name: string, req: CacheDeleteRequest) =>
    http.post<void>(`agents/${encodeURIComponent(name)}/cache/delete`, { json: req }),
}

/** 概览快照端点（后端 `api/overview.rs`，票 B5-T7，ADR-0019：概览页单一
 *  数据源——stat 卡全量真值 + 三类事实警示态 + 最近构建；任意登录角色）。 */
export const overviewApi = {
  /** 概览快照：同一份数后端也灌 /metrics（双消费）。 */
  snapshot: () => http.get<OverviewSnapshotResponse>('overview'),
}

/** 项目端点（后端 `api/projects.rs`；建项目为全局 admin 专属）。 */
export const projectsApi = {
  /** 项目清单（按可见性过滤：全局 admin 全量、普通用户仅有角色者）。 */
  list: () => http.get<ProjectResponse[]>('projects'),

  /** 项目详情（viewer 档）。 */
  get: (name: string) => http.get<ProjectResponse>(`projects/${encodeURIComponent(name)}`),

  /** 建项目（git/svn + 仓库 URL + 可选默认分支 + 可选 SCM 凭据）。 */
  create: (req: CreateProjectRequest) =>
    http.post<ProjectResponse>('projects', { json: req }),

  /** 测试连接（创建期，全局 admin，ad-hoc 凭据不落库）：ls-remote/info 探测
   *  返回当前 head；不阻塞保存，失败可读错误（凭据不回显，B5-T3）。 */
  scmProbe: (req: ScmProbeRequest) =>
    http.post<ScmProbeResponse>('projects/scm-probe', { json: req }),

  /** 分支枚举（git，创建期预填默认分支）：ls-remote --heads + --symref HEAD
   *  解析默认分支，供新建项目默认分支预填（ADR-0016）。 */
  scmBranches: (req: ScmBranchesRequest) =>
    http.post<ScmBranchesResponse>('projects/scm-branches', { json: req }),

  /** 既有项目测试连接（项目 admin，存储凭据）：解密项目 SCM 凭据探测 head。 */
  testConnection: (name: string) =>
    http.post<ScmProbeResponse>(`projects/${encodeURIComponent(name)}/test-connection`),

  /** 设置/清空 SCM 凭据（项目 admin，加密落库；username + password 皆空 = 清）。 */
  putScmCredential: (name: string, req: ScmCredentialRequest) =>
    http.put<void>(`projects/${encodeURIComponent(name)}/scm-credential`, { json: req }),

  /** 查看项目成员（项目 admin 档）。 */
  listMembers: (name: string) =>
    http.get<MemberResponse[]>(`projects/${encodeURIComponent(name)}/members`),

  /** 整组分配项目成员（PUT 语义：提交清单即完整状态，未列入者移除）。 */
  replaceMembers: (name: string, assignments: MemberAssignment[]) =>
    http.put<MemberResponse[]>(`projects/${encodeURIComponent(name)}/members`, {
      json: assignments,
    }),

  /** 读 pipeline 定义（viewer 档；项目详情 pipeline 列表的降级探测源——
   *  后端暂无 pipeline 列表端点，B4-T3 以定义 GET 探测 + 显式标注退化，
   *  端点交付后换真列表）。 */
  getPipeline: (name: string, pipeline: string) =>
    http.get<PipelineDefinitionResponse>(
      `projects/${encodeURIComponent(name)}/pipelines/${encodeURIComponent(pipeline)}`,
    ),
}

/** 用户端点（后端 `api/users.rs`，ADR-0014）。
 *  - 目录（项目 admin 档）：成员分配下拉。
 *  - 管理（全局 admin 专属）：全量列表 / 建号 / 禁用启用 / 代办重置密码。 */
export const usersApi = {
  /** 最小用户目录（仅 id + 用户名，排除已禁用；成员分配下拉源）。 */
  directory: () => http.get<DirectoryEntryResponse[]>('users/directory'),

  /** 全量用户列表（全局 admin；按用户名排序，含已禁用，无密码哈希）。 */
  list: () => http.get<UserResponse[]>('users'),

  /** 建号（全局 admin；is_admin 默认 false——admin 是建号时的显式选择）。 */
  create: (req: CreateUserRequest) =>
    http.post<UserResponse>('users', { json: req }),

  /** 禁用 / 启用（全局 admin；禁用同秒删其全部 session 与 PAT）。
   *  后端 `PatchUserRequest` 仅 disabled——切换已有用户 admin 的端点尚未交付。 */
  patch: (name: string, req: PatchUserRequest) =>
    http.patch<UserResponse>(`users/${encodeURIComponent(name)}`, { json: req }),

  /** 代办重置密码（全局 admin；覆写密码哈希，旧密码即刻失效）。 */
  resetPassword: (name: string, req: ResetPasswordRequest) =>
    http.put<void>(`users/${encodeURIComponent(name)}/password`, { json: req }),
}

/** 机密端点（后端 `api/secrets.rs`，ADR-0015）：值只写不读——列名 / 建覆写 / 删。
 *  项目 admin 档（全局 admin 隐含项目 admin，ADR-0014），viewer/runner 连名 403。
 *  机密名取自路径段（非请求体），env 键字符集；值永无读回端点。 */
export const secretsApi = {
  /** 列项目机密名（按名排序；值形态任何端点不回显）。 */
  list: (project: string) =>
    http.get<SecretNameResponse[]>(`projects/${encodeURIComponent(project)}/secrets`),

  /** 建/覆写机密（同名即覆写，成功 204 无值形态）。 */
  put: (project: string, secret: string, req: PutSecretRequest) =>
    http.put<void>(
      `projects/${encodeURIComponent(project)}/secrets/${encodeURIComponent(secret)}`,
      { json: req },
    ),

  /** 删除机密（名消失即 DELETE 后的可观察语义，成功 204）。 */
  delete: (project: string, secret: string) =>
    http.del<void>(
      `projects/${encodeURIComponent(project)}/secrets/${encodeURIComponent(secret)}`,
    ),
}

/** 审计端点（后端 `api/audit.rs`，ADR-0015）：`GET /audit`，仅全局 admin。
 *  按时间/用户/项目/事件类型过滤 + limit/offset 分页，时间倒序（后端保证）。
 *  响应为审计条目数组（无 total——下一页可用性由调用侧按条数 == limit 判定）。 */
export const auditApi = {
  /** 审计回放：过滤 + 分页（时间倒序；detail 为 JSON 对象，机密事件只记名）。 */
  list: (query: AuditQuery) => http.get<AuditEntryResponse[]>('audit', { query }),
}

/** PAT 端点（后端 `api/tokens.rs`，ADR-0014）：权限 = owner 本人（v1 无 scope 细分）。
 *  创建响应一次性返回完整令牌（此后任何端点不再回显）；列表无值形态；吊销删行。 */
export const tokensApi = {
  /** 列当前用户全部 PAT（按创建时间升序；名/创建时间/过期，永不含令牌值）。 */
  list: () => http.get<TokenResponse[]>('auth/tokens'),

  /** 创建 PAT（响应一次性返回完整令牌——明文仅此一次，立即保存）。 */
  create: (req: CreateTokenRequest) =>
    http.post<CreatedTokenResponse>('auth/tokens', { json: req }),

  /** 吊销 PAT（删行，下一请求即 401；他人 id 一律 404，不暴露存在性）。 */
  revoke: (id: number) => http.del<void>(`auth/tokens/${id}`),
}

/** Pipeline 定义端点（后端 `api/pipelines.rs`）。 */
export const pipelinesApi = {
  /** 读 pipeline 定义 + 修订版本（构建详情页触发参数/等待标签态/产物声明
   *  的只读消费面，ADR-0009 model 为单一事实源）。 */
  getDefinition: (project: string, pipeline: string) =>
    http.get<PipelineDefinitionResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}`,
    ),

  /** 保存 pipeline 定义（项目 admin 档，票 B4-T8 编辑器保存）：原样提交 model
   *  JSON（定义本体是 sisyphus-model 的 JSON 形态，server 解析 + model 校验，
   *  ADR-0009）；model 校验失败 422 + `detail.errors` 错误清单整组透传，成功
   *  返回新修订版本（首存 1、续存 +1，操作人为认证用户实名）。 */
  saveDefinition: (project: string, pipeline: string, definition: unknown) =>
    http.put<SaveDefinitionResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}`,
      { json: definition },
    ),
}

/** 构建端点（后端 `api/builds.rs`，ADR-0006/0008/0013）。 */
export const buildsApi = {
  /** 手动触发构建（runner 档）：参数覆盖 + 可选分支/commit/revision，
   *  返回 202 + 构建号（异步推进）。 */
  trigger: (
    project: string,
    pipeline: string,
    req: TriggerBuildRequest,
  ) =>
    http.post<BuildAcceptedResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds`,
      { json: req },
    ),

  /** 取消构建（runner 档，build 级）：终态幂等 202。 */
  cancel: (project: string, pipeline: string, number: number) =>
    http.post<BuildAcceptedResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}/cancel`,
    ),

  /** 重跑构建（runner 档）：from_scratch 新号 / from_failed 同号 attempt+1；
   *  非失败终态 from_failed 409。 */
  rerun: (project: string, pipeline: string, number: number, req: RerunBuildRequest) =>
    http.post<BuildAcceptedResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}/rerun`,
      { json: req },
    ),

  /** 构建列表（viewer 档）：按号倒序 + 分页(page/limit) + 状态过滤。 */
  list: (
    project: string,
    pipeline: string,
    query: { page?: number; limit?: number; status?: BuildStatusDto | '' },
  ) =>
    http.get<BuildListResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds`,
      { query },
    ),

  /** 构建详情（viewer 档）：状态/触发人/attempt/耗时/阶段与任务状态。 */
  detail: (project: string, pipeline: string, number: number) =>
    http.get<BuildDetailResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}`,
    ),

  /** 手动删构建（项目 admin 档，ADR-0013）：立即全删该构建的日志与产物
   *  （构建记录保留）；运行中/排队 409。成功 204。 */
  remove: (project: string, pipeline: string, number: number) =>
    http.del<void>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}`,
    ),
}

/** 产物端点（后端 `api/artifacts.rs`，票 #74 / B5-T2，ADR-0004/0007）：构建
 *  产物列表（详情页产物区）与单产物下载（cookie 会话随 `<a href>` 自动携
 *  带，浏览器原生下载）。 */
export const artifactsApi = {
  /** 构建产物列表（viewer 档）：任务声明展示与已上传产物匹配的比对源。 */
  list: (project: string, pipeline: string, number: number) =>
    http.get<BuildArtifactsResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}/artifacts`,
    ),

  /** 单产物下载 URL（相对路径——cookie 会话随同源导航自动携带；响应头带
   *  大小与校验和，浏览器原生下载）。 */
  downloadUrl: (project: string, pipeline: string, number: number, name: string) =>
    `api/v1/projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}/builds/${number}/artifacts/${encodeURIComponent(name)}`,
}

/** 升级包端点（后端 `api/upgrade_packages.rs`，票 #76 / B5-T4，ADR-0017）：
 *  管理面全局 admin（上传/列表/删除）；下载走 Agent token 鉴权，不经本客户端
 *  （由 Agent 侧拉取，UI 不提供浏览器下载）。一次多包 = 连续多次上传。 */
export const upgradePackagesApi = {
  /** 上传升级包（全局 admin，raw octet body + X-Sisyphus-Filename 头携带包名；
   *  后端按 ADR-0010 文件名规范解析版本/目标三元组、窗口校验、记 sha256）。 */
  upload: (file: File) =>
    http.post<UpgradePackageResponse>('upgrade-packages', {
      body: file,
      headers: { 'X-Sisyphus-Filename': file.name, 'Content-Type': 'application/octet-stream' },
    }),

  /** 升级包清单（全局 admin；按包名排序）。 */
  list: () => http.get<UpgradePackageResponse[]>('upgrade-packages'),

  /** 删除升级包（全局 admin；删旧包——元数据 + 字节）。 */
  delete: (packageName: string) =>
    http.del<void>(`upgrade-packages/${encodeURIComponent(packageName)}`),
}

// 端点级 API 客户端（票 B4-T1：工程底座把认证面端点立起来，其余端点在
// 各页面票按需扩展）。消费既有 REST 契约（ADR-0005：`/api/v1/` 前缀），
// 错误形态统一由 `http.ts` 落 `ApiError`（code/message/detail，按 code 分支）。

import { http } from './http-singleton'
import type { CredentialsRequest, MeResponse } from './http'
import type {
  AgentResponse,
  BuildAcceptedResponse,
  BuildDetailResponse,
  BuildListResponse,
  BuildStatusDto,
  CreateAgentRequest,
  CreateProjectRequest,
  CreatedAgentResponse,
  DirectoryEntryResponse,
  MemberAssignment,
  MemberResponse,
  PatchAgentRequest,
  PipelineDefinitionResponse,
  ProjectResponse,
  RerunBuildRequest,
  TriggerBuildRequest,
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
}

/** 项目端点（后端 `api/projects.rs`；建项目为全局 admin 专属）。 */
export const projectsApi = {
  /** 项目清单（按可见性过滤：全局 admin 全量、普通用户仅有角色者）。 */
  list: () => http.get<ProjectResponse[]>('projects'),

  /** 项目详情（viewer 档）。 */
  get: (name: string) => http.get<ProjectResponse>(`projects/${encodeURIComponent(name)}`),

  /** 建项目（git/svn + 仓库 URL + 可选默认分支）。 */
  create: (req: CreateProjectRequest) =>
    http.post<ProjectResponse>('projects', { json: req }),

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

/** 用户目录端点（后端 `api/users.rs`；项目 admin 档，成员分配下拉）。 */
export const usersApi = {
  /** 最小用户目录（仅 id + 用户名，排除已禁用；成员分配下拉源）。 */
  directory: () => http.get<DirectoryEntryResponse[]>('users/directory'),
}

/** Pipeline 定义端点（后端 `api/pipelines.rs`）。 */
export const pipelinesApi = {
  /** 读 pipeline 定义 + 修订版本（构建详情页触发参数/等待标签态/产物声明
   *  的只读消费面，ADR-0009 model 为单一事实源）。 */
  getDefinition: (project: string, pipeline: string) =>
    http.get<PipelineDefinitionResponse>(
      `projects/${encodeURIComponent(project)}/pipelines/${encodeURIComponent(pipeline)}`,
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
}

// 端点级 API 客户端（票 B4-T1：工程底座把认证面端点立起来，其余端点在
// 各页面票按需扩展）。消费既有 REST 契约（ADR-0005：`/api/v1/` 前缀），
// 错误形态统一由 `http.ts` 落 `ApiError`（code/message/detail，按 code 分支）。

import { http } from './http-singleton'
import type { CredentialsRequest, MeResponse } from './http'
import type {
  AgentResponse,
  CreateAgentRequest,
  CreateProjectRequest,
  CreatedAgentResponse,
  DirectoryEntryResponse,
  MemberAssignment,
  MemberResponse,
  PipelineDefinitionResponse,
  ProjectResponse,
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

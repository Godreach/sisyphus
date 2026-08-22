// Agent 四态派生（ADR-0008/0017，票 B4-T5/B5-T4）：列表/详情徽标的纯派生逻辑。
//
// ADR-0017 四态 = 在线 / 离线 / 排空 / 版本不兼容，由「在线状态 + 排空标志 +
// 版本窗口字段」派生。REST `AgentResponse` 契约自 B5-T4 起暴露 `draining` 与
// `version_compatible`——四态全可派生。
//
// 优先级（停用独立管理态、最高）：disabled > incompatible > draining > online
// > offline。过旧 Agent（version_compatible=false）即便在线也标「版本不兼容」
// （任务面拒连、升级面保留）；排空中 Agent（online + draining）标「排空」。
//
// 停用（`disabled`）是独立的管理态（AC 单列「停用/启用」），不在四态内：
// `agentBadgeState` 在停用时优先返回 `'disabled'`——停用即踢线（ADR-0008），
// 停用的 Agent 即便 `online` 残真也不展示「在线」以免误导。

import type { AgentResponse } from '@/api/types'

/** ADR-0017 四态（在线 / 离线 / 排空 / 版本不兼容）。 */
export type AgentOperationalState = 'online' | 'offline' | 'draining' | 'incompatible'

/** 列表/详情徽标态：四态 + 停用（停用优先，独立管理态）。 */
export type AgentBadgeState = AgentOperationalState | 'disabled'

/**
 * ADR-0017 四态派生（operational，不含停用）。优先级 incompatible > draining
 * > online > offline：过旧 Agent 标「版本不兼容」（即便在线）；在线且排空中
 * 标「排空」；否则按在线态。
 */
export function deriveAgentState(
  agent: Pick<AgentResponse, 'online' | 'draining' | 'version_compatible'>,
): AgentOperationalState {
  if (!agent.version_compatible) return 'incompatible'
  if (agent.online && agent.draining) return 'draining'
  return agent.online ? 'online' : 'offline'
}

/**
 * 徽标态：停用优先（停用即踢线，停用 Agent 不展示在线态），否则取四态派生。
 */
export function agentBadgeState(
  agent: Pick<AgentResponse, 'online' | 'disabled' | 'draining' | 'version_compatible'>,
): AgentBadgeState {
  if (agent.disabled) return 'disabled'
  return deriveAgentState(agent)
}

/** 徽标态 → i18n key（`agents.stateOnline` / `stateOffline` / … , `stateDisabled`）。 */
export function agentStateLabelKey(state: AgentBadgeState): string {
  const cap = state.charAt(0).toUpperCase() + state.slice(1)
  return `agents.state${cap}`
}

/** 徽标态 → CSS 类后缀（`agent-state-online` 等，配 main.css 状态色）。 */
export function agentStateClass(state: AgentBadgeState): string {
  return `agent-state-${state}`
}

/** 徽标态 → NTag 状态色（票 #94 列表/详情共用，颜色编码不漂移）：
 *  online=绿 / offline=红 / draining=黄 / incompatible=灰；停用（独立
 *  管理态）同为灰，靠文案区分。 */
export function agentStateTagType(
  state: AgentBadgeState,
): 'success' | 'error' | 'warning' | 'default' {
  switch (state) {
    case 'online':
      return 'success'
    case 'offline':
      return 'error'
    case 'draining':
      return 'warning'
    default:
      return 'default'
  }
}

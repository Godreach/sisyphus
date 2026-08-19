// Agent 四态派生（ADR-0008/0017，票 B4-T5）：列表/详情徽标的纯派生逻辑。
//
// ADR-0017 四态 = 在线 / 离线 / 排空 / 版本不兼容，由「在线状态 + 排空标志 +
// 版本窗口字段」派生。当前 REST `AgentResponse` 契约**仅暴露 `online`**
// （排空标志 / 版本窗口字段尚未进 REST 契约——B4 纯前端消费既有契约，不补
// 后端，立票跟进），故今日仅由 `online` 派生「在线 / 离线」；排空 / 不兼容
// 两个态当前不可派生（页面以退化态显式标注「依赖后端排空/版本字段，尚未
// 交付」，与概览页 alert-degraded 同纪律）。后端补字段后在此接上判定——
// 函数签名已返回四态联合，是前向兼容缝。
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
 * ADR-0017 四态派生（operational，不含停用）。今日仅由 `online` 派生
 * 在线 / 离线；排空 / 不兼容 待后端补排空标志 + 版本窗口字段后接上。
 */
export function deriveAgentState(
  agent: Pick<AgentResponse, 'online'>,
): AgentOperationalState {
  return agent.online ? 'online' : 'offline'
}

/**
 * 徽标态：停用优先（停用即踢线，停用 Agent 不展示在线态），否则取四态派生。
 */
export function agentBadgeState(
  agent: Pick<AgentResponse, 'online' | 'disabled'>,
): AgentBadgeState {
  if (agent.disabled) return 'disabled'
  return deriveAgentState(agent)
}

/** 徽标态 → i18n key（`agents.stateOnline` / `stateOffline` / … / `stateDisabled`）。 */
export function agentStateLabelKey(state: AgentBadgeState): string {
  const cap = state.charAt(0).toUpperCase() + state.slice(1)
  return `agents.state${cap}`
}

/** 徽标态 → CSS 类后缀（`agent-state-online` 等，配 main.css 状态色）。 */
export function agentStateClass(state: AgentBadgeState): string {
  return `agent-state-${state}`
}

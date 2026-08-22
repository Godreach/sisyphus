// ADR-0017 四态派生纯逻辑单测（票 B4-T5/B5-T4）：只测派生函数的外部行为。
// - deriveAgentState：由 online + draining + version_compatible 派生四态
//   （incompatible > draining > online > offline）。
// - agentBadgeState：停用优先（停用即踢线，不展示在线态以免误导）。
// - agentStateLabelKey / agentStateClass：徽标态 → i18n key / CSS 类。
// - agentStateTagType：徽标态 → NTag 状态色（票 #94 AC 颜色编码）。

import { describe, expect, it } from 'vitest'

import {
  agentBadgeState,
  agentStateClass,
  agentStateLabelKey,
  agentStateTagType,
  deriveAgentState,
} from '@/utils/agentState'
import type { AgentResponse } from '@/api/types'

/** 默认可派发 Agent（在线 + 兼容 + 非排空）——用 overrides 切换四态。 */
function base(overrides: Partial<AgentResponse> = {}): Pick<
  AgentResponse,
  'online' | 'draining' | 'version_compatible' | 'disabled'
> {
  return { online: true, draining: false, version_compatible: true, disabled: false, ...overrides }
}

describe('deriveAgentState 四态派生', () => {
  it('在线 + 兼容 + 非排空 → online', () => {
    expect(deriveAgentState(base())).toBe('online')
  })

  it('离线 + 兼容 → offline（非排空、非不兼容）', () => {
    expect(deriveAgentState(base({ online: false }))).toBe('offline')
  })

  it('在线 + 排空 → draining（在线但不可派发）', () => {
    expect(deriveAgentState(base({ draining: true }))).toBe('draining')
  })

  it('离线 + 排空 → offline（排空需在线才显示；离线即离线）', () => {
    expect(deriveAgentState(base({ online: false, draining: true }))).toBe('offline')
  })

  it('版本不兼容 → incompatible（即便在线）', () => {
    expect(deriveAgentState(base({ version_compatible: false }))).toBe('incompatible')
  })

  it('版本不兼容 + 离线 → incompatible（不兼容优先于离线，ADR-0017）', () => {
    expect(deriveAgentState(base({ online: false, version_compatible: false }))).toBe(
      'incompatible',
    )
  })

  it('不兼容优先于排空（过旧 + 排空 → incompatible）', () => {
    expect(
      deriveAgentState(base({ draining: true, version_compatible: false })),
    ).toBe('incompatible')
  })
})

describe('agentBadgeState 停用优先', () => {
  it('disabled=true 且在线 → disabled（停用即踢线，不展示在线）', () => {
    expect(agentBadgeState(base({ disabled: true }))).toBe('disabled')
  })

  it('disabled=true 且离线 → disabled', () => {
    expect(agentBadgeState(base({ online: false, disabled: true }))).toBe('disabled')
  })

  it('disabled=false 在线 → online', () => {
    expect(agentBadgeState(base())).toBe('online')
  })

  it('disabled=false 离线 → offline', () => {
    expect(agentBadgeState(base({ online: false }))).toBe('offline')
  })

  it('停用优先于不兼容/排空', () => {
    expect(
      agentBadgeState(base({ disabled: true, version_compatible: false, draining: true })),
    ).toBe('disabled')
  })
})

describe('agentStateLabelKey / agentStateClass', () => {
  it('label key 形态：agents.state{Online|Offline|Draining|Incompatible|Disabled}', () => {
    expect(agentStateLabelKey('online')).toBe('agents.stateOnline')
    expect(agentStateLabelKey('offline')).toBe('agents.stateOffline')
    expect(agentStateLabelKey('draining')).toBe('agents.stateDraining')
    expect(agentStateLabelKey('incompatible')).toBe('agents.stateIncompatible')
    expect(agentStateLabelKey('disabled')).toBe('agents.stateDisabled')
  })

  it('class 形态：agent-state-{state}', () => {
    expect(agentStateClass('online')).toBe('agent-state-online')
    expect(agentStateClass('draining')).toBe('agent-state-draining')
    expect(agentStateClass('incompatible')).toBe('agent-state-incompatible')
    expect(agentStateClass('disabled')).toBe('agent-state-disabled')
  })
})

describe('agentStateTagType NTag 状态色（票 #94）', () => {
  it('online=绿 / offline=红 / draining=黄 / incompatible=灰', () => {
    expect(agentStateTagType('online')).toBe('success')
    expect(agentStateTagType('offline')).toBe('error')
    expect(agentStateTagType('draining')).toBe('warning')
    expect(agentStateTagType('incompatible')).toBe('default')
  })

  it('停用（独立管理态）同为灰，靠文案区分', () => {
    expect(agentStateTagType('disabled')).toBe('default')
  })
})

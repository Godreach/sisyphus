// ADR-0017 四态派生纯逻辑单测（票 B4-T5）：只测派生函数的外部行为。
// - deriveAgentState：由 online 派生 在线/离线（排空/不兼容 当前 REST 契约
//   未暴露排空/版本字段 → 不可派生，固化「回落 online/offline，不误报」）。
// - agentBadgeState：停用优先（停用即踢线，不展示在线态以免误导）。
// - agentStateLabelKey / agentStateClass：徽标态 → i18n key / CSS 类。

import { describe, expect, it } from 'vitest'

import {
  agentBadgeState,
  agentStateClass,
  agentStateLabelKey,
  deriveAgentState,
} from '@/utils/agentState'

describe('deriveAgentState 四态派生', () => {
  it('online=true → online', () => {
    expect(deriveAgentState({ online: true })).toBe('online')
  })

  it('online=false → offline', () => {
    expect(deriveAgentState({ online: false })).toBe('offline')
  })

  // 排空 / 不兼容：ADR-0017 由排空标志 + 版本窗口字段派生，当前 REST
  // AgentResponse 契约尚未暴露这两个字段——今日不可派生（页面以退化态显式
  // 标注）。此处固化「字段缺省时回落 online/offline」，绝不误报排空/不兼容。
  it('排空 / 不兼容 当前不可派生：仅 online 字段时不误报', () => {
    expect(deriveAgentState({ online: true })).not.toBe('draining')
    expect(deriveAgentState({ online: true })).not.toBe('incompatible')
    expect(deriveAgentState({ online: false })).not.toBe('draining')
    expect(deriveAgentState({ online: false })).not.toBe('incompatible')
  })
})

describe('agentBadgeState 停用优先', () => {
  it('disabled=true 且 online=true → disabled（停用即踢线，不展示在线）', () => {
    expect(agentBadgeState({ online: true, disabled: true })).toBe('disabled')
  })

  it('disabled=true 且 online=false → disabled', () => {
    expect(agentBadgeState({ online: false, disabled: true })).toBe('disabled')
  })

  it('disabled=false online=true → online', () => {
    expect(agentBadgeState({ online: true, disabled: false })).toBe('online')
  })

  it('disabled=false online=false → offline', () => {
    expect(agentBadgeState({ online: false, disabled: false })).toBe('offline')
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
    expect(agentStateClass('disabled')).toBe('agent-state-disabled')
  })
})

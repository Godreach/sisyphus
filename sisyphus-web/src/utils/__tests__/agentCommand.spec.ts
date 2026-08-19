// 按目标 OS 的注册命令构建纯逻辑单测（票 B4-T5）：
// - linux/macos：无 .exe、含 --reg-key 换码、双端口（gRPC 50051 / REST 8080）
// - windows：二进制带 .exe
// 与 SetupView 既有断言同形态（抽取自 SetupView，行为不变）。

import { describe, expect, it } from 'vitest'

import { buildAgentRegisterCommand } from '@/utils/agentCommand'

describe('buildAgentRegisterCommand', () => {
  it('linux：无 .exe、含 --reg-key 换码、双端口占位', () => {
    const cmd = buildAgentRegisterCommand('linux', 'sisa_reg_C0D3')
    expect(cmd).toContain('sisyphus-agent')
    expect(cmd).not.toContain('.exe')
    expect(cmd).toContain('--reg-key sisa_reg_C0D3')
    expect(cmd).toContain('--server-url http://<server>:50051')
    expect(cmd).toContain('--api-url http://<server>:8080')
  })

  it('macos 与 linux 同形（无 .exe）', () => {
    expect(buildAgentRegisterCommand('macos', 'x')).not.toContain('.exe')
  })

  it('windows：二进制带 .exe', () => {
    expect(buildAgentRegisterCommand('windows', 'x')).toContain('sisyphus-agent.exe')
  })
})

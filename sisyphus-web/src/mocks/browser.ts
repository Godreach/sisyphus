// MSW dev worker 入口（ADR-0024，票 #101）：`VITE_ENABLE_MOCK=1` 时由
// main.ts 动态加载（mock 层不进构建产物——tree-shake 掉动态分支即可，
// 即使进了也不在关闭开关时执行）。authEnforced=true：登录/会话 cookie 生效。

import { setupWorker } from 'msw/browser'

import { createHandlers } from './handlers'
import { installMockEventSource } from './eventSource'

export async function startMockWorker(): Promise<void> {
  installMockEventSource()
  const worker = setupWorker(...createHandlers({ authEnforced: true }))
  await worker.start({ onUnhandledRequest: 'bypass', quiet: true })
}

// MSW node 入口（ADR-0024，票 #101）：vitest 组件挂载测试消费——组件经
// 真实 http client 打本 server，fixture 即测试数据（单一缝，取代逐 spec
// 手写 fetch mock）。authEnforced=false：挂载测试不走登录链路，直连放行。

import { setupServer } from 'msw/node'

import { createHandlers } from './handlers'

export const server = setupServer(...createHandlers({ authEnforced: false }))

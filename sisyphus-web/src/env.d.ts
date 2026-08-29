/// <reference types="vite/client" />

// 环境开关（ADR-0024，票 #101）：`VITE_ENABLE_MOCK=1` 时 dev 挂载 MSW
// worker 与 SSE 日志流替身（src/mocks/）；关闭走 vite proxy 连真后端。
interface ImportMetaEnv {
  readonly VITE_ENABLE_MOCK?: string
}

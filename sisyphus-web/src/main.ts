// 应用入口（ADR-0003，票 B4-T1）：
// - 挂载前先 await 会话恢复（`/auth/me` 锚点）：刷新/深链直接命中，
//   避免「先渲染受保护页再弹登录」的闪烁。
// - Pinia 在 useAuthStore 之前实例化（auth store 在守卫里消费）。
// - mock 开关（ADR-0024，票 #101）：`VITE_ENABLE_MOCK=1` 时在 dev 环境
//   会话恢复前挂载 MSW worker 与 SSE 日志流替身（DEV 守卫：生产构建一律
//   不挂载，mock 层不进产物）；关闭（缺省）走 vite proxy 连真后端，行为
//   与现状一致。

import { createApp } from 'vue'
import { createPinia, setActivePinia } from 'pinia'

import App from './App.vue'
import { router } from './router'
import { i18n } from './i18n'
import './assets/main.css'

import { useAuthStore } from './stores/auth'

async function bootstrap(): Promise<void> {
  // mock 层在会话恢复前挂载：/auth/me 探测也走 mock（登录态由 mock 会话决定）。
  if (import.meta.env.DEV && import.meta.env.VITE_ENABLE_MOCK === '1') {
    const { startMockWorker } = await import('./mocks/browser')
    await startMockWorker()
  }

  const app = createApp(App)
  const pinia = createPinia()
  app.use(pinia)
  // 挂载前组件上下文外调用 store：需先设活动 pinia（组件内 useStore 走
  // 注入不受影响）。
  setActivePinia(pinia)

  // 会话恢复锚点：挂载路由前先判定认证态（首次导航守卫不再重复探测）。
  const auth = useAuthStore()
  await auth.restore()

  app.use(router)
  app.use(i18n)
  app.mount('#app')
}

void bootstrap()

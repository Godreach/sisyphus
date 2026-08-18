// 应用入口（ADR-0003，票 B4-T1）：
// - 挂载前先 await 会话恢复（`/auth/me` 锚点）：刷新/深链直接命中，
//   避免「先渲染受保护页再弹登录」的闪烁。
// - Pinia 在 useAuthStore 之前实例化（auth store 在守卫里消费）。

import { createApp } from 'vue'
import { createPinia, setActivePinia } from 'pinia'

import App from './App.vue'
import { router } from './router'
import { i18n } from './i18n'
import './assets/main.css'

import { useAuthStore } from './stores/auth'

async function bootstrap(): Promise<void> {
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

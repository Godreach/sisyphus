// i18n 装配（ADR-0003/0020：vue-i18n v11，zh 为源语言、en 全量对译；
// catalog key 集合一致性由 `npm run i18n:check` 脚本在 CI 强制）。
//
// 源语言 zh-CN 也注入 messages：未切换语言时同样可渲染（单实例 i18n，
// fallback 到 zh 避免 en 缺 key 白屏）。locale 持久化在 localStorage，
// 底部 zh/EN 即时切换（App 壳挂全局按钮）。

import { createI18n } from 'vue-i18n'

import zhCN from './locales/zh-CN.json'
import enUS from './locales/en-US.json'

export type Locale = 'zh-CN' | 'en-US'

const STORAGE_KEY = 'sisyphus.locale'

function initialLocale(): Locale {
  if (typeof window === 'undefined') return 'zh-CN'
  const stored = window.localStorage.getItem(STORAGE_KEY)
  if (stored === 'zh-CN' || stored === 'en-US') return stored
  // 浏览器语言偏好里含中文则默认 zh，否则 en（公众产品默认语言跟随系统）。
  return navigator.language.toLowerCase().startsWith('zh') ? 'zh-CN' : 'en-US'
}

export const i18n = createI18n({
  legacy: false,
  globalInjection: true,
  locale: initialLocale(),
  fallbackLocale: 'zh-CN',
  messages: {
    'zh-CN': zhCN,
    'en-US': enUS,
  },
})

/** 当前语言（供语言切换按钮与持久化）。 */
export function currentLocale(): Locale {
  return i18n.global.locale.value as Locale
}

/** 切换语言并持久化（App 壳底部按钮消费）。 */
export function setLocale(locale: Locale): void {
  i18n.global.locale.value = locale
  if (typeof window !== 'undefined') {
    window.localStorage.setItem(STORAGE_KEY, locale)
  }
  document.documentElement.lang = locale === 'zh-CN' ? 'zh-CN' : 'en'
}

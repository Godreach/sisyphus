import { computed, onScopeDispose, ref, watchEffect } from 'vue'
import { darkTheme, type GlobalThemeOverrides } from 'naive-ui'
import { themeOverrides, darkThemeOverrides } from '@/theme'

/** 主题偏好（票 #104 裁定 G4）：'system' 跟随系统（spec #100 story 25 行为，
 *  默认值）；'light'/'dark' 手动覆盖。持久化 localStorage；旧值 'light'/'dark'
 *  （登录页开关时代）语义不变。 */
export type ThemePreference = 'system' | 'light' | 'dark'

const THEME_PREF_KEY = 'sisyphus-theme'

function readPreference(): ThemePreference {
  try {
    const v = localStorage.getItem(THEME_PREF_KEY)
    if (v === 'light' || v === 'dark' || v === 'system') return v
  } catch {
    // localStorage 不可用（隐私模式等）：回落跟随系统。
  }
  return 'system'
}

/** 偏好值模块级共享（App 壳单实例消费；用户卡菜单写入后全应用即时生效）。 */
const preference = ref<ThemePreference>(readPreference())

export function useDarkMode() {
  const mql = window.matchMedia('(prefers-color-scheme: dark)')
  // 系统偏好转响应式：偏好为 system 时 isDark 跟随系统实时变化。
  const systemDark = ref(mql.matches)

  function handler(e: MediaQueryListEvent) {
    systemDark.value = e.matches
  }
  mql.addEventListener('change', handler)

  onScopeDispose(() => {
    mql.removeEventListener('change', handler)
  })

  const isDark = computed(() =>
    preference.value === 'system' ? systemDark.value : preference.value === 'dark',
  )

  // CSS 变量（--sisy-*）的深色块由 html[data-theme] 门控，与 JS 主题同源。
  // 跟随系统时不落 data-theme（CSS 侧同样走 prefers-color-scheme 媒体查询）。
  watchEffect(() => {
    if (preference.value === 'system') {
      delete document.documentElement.dataset.theme
    } else {
      document.documentElement.dataset.theme = preference.value
    }
  })

  /** 设置主题偏好并持久化（用户卡菜单三态消费）。 */
  function setPreference(mode: ThemePreference): void {
    try {
      localStorage.setItem(THEME_PREF_KEY, mode)
    } catch {
      // localStorage 不可用：偏好仅本次会话生效。
    }
    preference.value = mode
  }

  const theme = computed(() => (isDark.value ? darkTheme : null))

  const currentThemeOverrides = computed<GlobalThemeOverrides>(() =>
    isDark.value ? darkThemeOverrides : themeOverrides,
  )

  return { preference, isDark, theme, themeOverrides: currentThemeOverrides, setPreference }
}

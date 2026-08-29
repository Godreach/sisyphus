import { computed, onScopeDispose, ref, watchEffect } from 'vue'
import { darkTheme, type GlobalThemeOverrides } from 'naive-ui'
import { themeOverrides, darkThemeOverrides } from '@/theme'

/** 主题覆盖键（localStorage；'light' | 'dark'）。
 *  默认跟随系统；显式覆盖供 UI 开关（登录页主题切换）与预览/验收/调试使用。 */
const THEME_OVERRIDE_KEY = 'sisyphus-theme'

function readThemeOverride(): 'light' | 'dark' | null {
  try {
    const v = localStorage.getItem(THEME_OVERRIDE_KEY)
    return v === 'light' || v === 'dark' ? v : null
  } catch {
    return null
  }
}

/** 覆盖值模块级共享（App.vue 单实例消费；UI 开关写入后全应用即时生效）。 */
const override = ref<'light' | 'dark' | null>(readThemeOverride())

export function useDarkMode() {
  const mql = window.matchMedia('(prefers-color-scheme: dark)')
  // 系统偏好转响应式：无显式覆盖时 isDark 跟随系统实时变化。
  const systemDark = ref(mql.matches)

  function handler(e: MediaQueryListEvent) {
    systemDark.value = e.matches
  }
  mql.addEventListener('change', handler)

  onScopeDispose(() => {
    mql.removeEventListener('change', handler)
  })

  const isDark = computed(() =>
    override.value !== null ? override.value === 'dark' : systemDark.value,
  )

  // CSS 变量（--sisy-*）的深色块由 html[data-theme] 门控，与 JS 主题同源
  // （覆盖写入/清除即时落到 DOM）。
  watchEffect(() => {
    if (override.value !== null) {
      document.documentElement.dataset.theme = override.value
    } else {
      delete document.documentElement.dataset.theme
    }
  })

  /** 显式设置主题（'light' | 'dark'）；null = 清除覆盖、回落跟随系统。 */
  function setTheme(mode: 'light' | 'dark' | null): void {
    try {
      if (mode === null) localStorage.removeItem(THEME_OVERRIDE_KEY)
      else localStorage.setItem(THEME_OVERRIDE_KEY, mode)
    } catch {
      // localStorage 不可用（隐私模式等）：覆盖仅本次会话生效。
    }
    override.value = mode
  }

  const theme = computed(() => (isDark.value ? darkTheme : null))

  const currentThemeOverrides = computed<GlobalThemeOverrides>(() =>
    isDark.value ? darkThemeOverrides : themeOverrides,
  )

  return { isDark, theme, themeOverrides: currentThemeOverrides, setTheme }
}

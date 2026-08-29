import { computed, onScopeDispose, ref } from 'vue'
import { darkTheme, type GlobalThemeOverrides } from 'naive-ui'
import { themeOverrides, darkThemeOverrides } from '@/theme'

/** 主题覆盖键（localStorage；'light' | 'dark'）。
 *  ADR-0023 默认仍跟随系统、无 UI 开关；此键仅供预览/验收/调试显式覆盖。 */
const THEME_OVERRIDE_KEY = 'sisyphus-theme'

function readThemeOverride(): 'light' | 'dark' | null {
  try {
    const v = localStorage.getItem(THEME_OVERRIDE_KEY)
    return v === 'light' || v === 'dark' ? v : null
  } catch {
    return null
  }
}

export function useDarkMode() {
  const mql = window.matchMedia('(prefers-color-scheme: dark)')
  const override = readThemeOverride()
  const isDark = ref(override !== null ? override === 'dark' : mql.matches)

  // CSS 变量（--sisy-*）的深色块由 html[data-theme] 门控，与 JS 主题同源。
  if (override !== null) {
    document.documentElement.dataset.theme = override
  } else {
    delete document.documentElement.dataset.theme
  }

  function handler(e: MediaQueryListEvent) {
    // 显式覆盖时不跟随系统变化。
    if (readThemeOverride() !== null) return
    isDark.value = e.matches
  }
  mql.addEventListener('change', handler)

  onScopeDispose(() => {
    mql.removeEventListener('change', handler)
  })

  const theme = computed(() => (isDark.value ? darkTheme : null))

  const currentThemeOverrides = computed<GlobalThemeOverrides>(() =>
    isDark.value ? darkThemeOverrides : themeOverrides,
  )

  return { isDark, theme, themeOverrides: currentThemeOverrides }
}

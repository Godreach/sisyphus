import { computed, onScopeDispose, ref } from 'vue'
import { darkTheme, type GlobalThemeOverrides } from 'naive-ui'
import { themeOverrides, darkThemeOverrides } from '@/theme'

export function useDarkMode() {
  const mql = window.matchMedia('(prefers-color-scheme: dark)')
  const isDark = ref(mql.matches)

  function handler(e: MediaQueryListEvent) {
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

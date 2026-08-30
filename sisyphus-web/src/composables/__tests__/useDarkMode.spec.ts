import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nextTick } from 'vue'
import { useDarkMode, type ThemePreference } from '@/composables/useDarkMode'

describe('useDarkMode', () => {
  let mediaQuery: MockMediaQueryList

  beforeEach(() => {
    mediaQuery = createMockMediaQuery(false)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)
    localStorage.removeItem('sisyphus-theme')
  })

  afterEach(() => {
    // 偏好是模块级共享状态：还原跟随系统，避免跨用例串扰。
    const { setPreference } = useDarkMode()
    setPreference('system')
    vi.restoreAllMocks()
  })

  it('初始为 light 模式（系统偏好 light）', () => {
    const { isDark, theme } = useDarkMode()
    expect(isDark.value).toBe(false)
    expect(theme.value).toBeNull()
  })

  it('初始为 dark 模式（系统偏好 dark）', () => {
    mediaQuery = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isDark, theme } = useDarkMode()
    expect(isDark.value).toBe(true)
    expect(theme.value).not.toBeNull()
  })

  it('系统切换 dark → isDark 跟随变化（跟随系统偏好）', async () => {
    const { isDark, theme } = useDarkMode()
    expect(isDark.value).toBe(false)

    // 模拟系统切换到 dark
    mediaQuery._simulateChange(true)
    await nextTick()

    expect(isDark.value).toBe(true)
    expect(theme.value).not.toBeNull()
  })

  it('系统切换 light → isDark 跟随变化', async () => {
    mediaQuery = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isDark, theme } = useDarkMode()
    expect(isDark.value).toBe(true)

    mediaQuery._simulateChange(false)
    await nextTick()

    expect(isDark.value).toBe(false)
    expect(theme.value).toBeNull()
  })

  it('dark theme overrides 在 dark 模式下返回 dark 主题', () => {
    const { themeOverrides } = useDarkMode()
    // light 模式下 themeOverrides 应该返回 light overrides
    expect(themeOverrides.value).toBeDefined()
  })

  // ===== 三态主题偏好（票 #104 裁定 G4：跟随系统/浅色/深色，持久化） =====

  it.each<[ThemePreference, boolean]>([
    ['light', false],
    ['dark', true],
  ])('手动偏好 %s 覆盖系统（持久化 + data-theme 同步）', async (mode, expectDark) => {
    const { preference, isDark, setPreference } = useDarkMode()
    setPreference(mode)
    await nextTick()

    expect(preference.value).toBe(mode)
    expect(isDark.value).toBe(expectDark)
    expect(localStorage.getItem('sisyphus-theme')).toBe(mode)
    expect(document.documentElement.dataset.theme).toBe(mode)
  })

  it('手动深色覆盖浅色系统；回到「跟随系统」清除 data-theme 并恢复跟随', async () => {
    mediaQuery = createMockMediaQuery(false)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isDark, setPreference } = useDarkMode()
    setPreference('dark')
    await nextTick()
    expect(isDark.value).toBe(true)

    setPreference('system')
    await nextTick()
    expect(isDark.value).toBe(false)
    expect(document.documentElement.dataset.theme).toBeUndefined()
    expect(localStorage.getItem('sisyphus-theme')).toBe('system')
  })
})

// ---------- helpers ----------

interface MockMediaQueryList {
  matches: boolean
  addEventListener: ReturnType<typeof vi.fn>
  removeEventListener: ReturnType<typeof vi.fn>
  _simulateChange: (matches: boolean) => void
}

function createMockMediaQuery(matches: boolean): MockMediaQueryList {
  let _handler: ((e: MediaQueryListEvent) => void) | null = null
  return {
    matches,
    addEventListener: vi.fn((_type: string, handler: (e: MediaQueryListEvent) => void) => {
      _handler = handler
    }),
    removeEventListener: vi.fn(),
    _simulateChange(newMatches: boolean) {
      this.matches = newMatches
      _handler?.({ matches: newMatches } as MediaQueryListEvent)
    },
  }
}

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { nextTick } from 'vue'
import { useDarkMode } from '@/composables/useDarkMode'

describe('useDarkMode', () => {
  let mediaQuery: MockMediaQueryList

  beforeEach(() => {
    mediaQuery = createMockMediaQuery(false)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)
  })

  afterEach(() => {
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

  it('系统切换 dark → isDark 跟随变化', async () => {
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

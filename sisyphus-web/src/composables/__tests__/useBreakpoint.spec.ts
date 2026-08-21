import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { effectScope, nextTick } from 'vue'
import { useBreakpoint } from '@/composables/useBreakpoint'

describe('useBreakpoint', () => {
  let mediaQuery: MockMediaQueryList

  beforeEach(() => {
    mediaQuery = createMockMediaQuery(false)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('默认 768px 断点：≥768 时 isNarrow 为 false', () => {
    mediaQuery = createMockMediaQuery(false)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isNarrow } = useBreakpoint()
    expect(isNarrow.value).toBe(false)
  })

  it('默认 768px 断点：<768 时 isNarrow 为 true', () => {
    mediaQuery = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isNarrow } = useBreakpoint()
    expect(isNarrow.value).toBe(true)
  })

  it('窗口从宽变窄 → isNarrow 跟随变化', async () => {
    const { isNarrow } = useBreakpoint()
    expect(isNarrow.value).toBe(false)

    mediaQuery._simulateChange(true)
    await nextTick()

    expect(isNarrow.value).toBe(true)
  })

  it('窗口从窄变宽 → isNarrow 跟随变化', async () => {
    mediaQuery = createMockMediaQuery(true)
    vi.spyOn(window, 'matchMedia').mockReturnValue(mediaQuery as unknown as MediaQueryList)

    const { isNarrow } = useBreakpoint()
    expect(isNarrow.value).toBe(true)

    mediaQuery._simulateChange(false)
    await nextTick()

    expect(isNarrow.value).toBe(false)
  })

  it('自定义断点值传入 matchMedia', () => {
    useBreakpoint(1024)
    expect(window.matchMedia).toHaveBeenCalledWith('(max-width: 1023px)')
  })

  it('scope 销毁后移除事件监听', () => {
    const scope = effectScope()
    scope.run(() => {
      useBreakpoint()
    })
    scope.stop()
    expect(mediaQuery.removeEventListener).toHaveBeenCalled()
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

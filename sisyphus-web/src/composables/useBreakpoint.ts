import { onScopeDispose, ref } from 'vue'

/**
 * 响应式断点检测： viewport 宽度 < breakpoint 时 isNarrow 为 true。
 * 默认断点 768px（侧栏折叠阈值）。
 */
export function useBreakpoint(breakpoint = 768) {
  const mql = window.matchMedia(`(max-width: ${breakpoint - 1}px)`)
  const isNarrow = ref(mql.matches)

  function handler(e: MediaQueryListEvent) {
    isNarrow.value = e.matches
  }
  mql.addEventListener('change', handler)

  onScopeDispose(() => {
    mql.removeEventListener('change', handler)
  })

  return { isNarrow }
}

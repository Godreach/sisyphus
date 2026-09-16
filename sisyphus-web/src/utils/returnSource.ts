import type { RouteLocationNormalizedLoaded, Router } from 'vue-router'
import { nextTick } from 'vue'

export function returnSourceQuery(
  route: RouteLocationNormalizedLoaded,
  router: Router,
  excludedQueryKeys: string[] = [],
): Record<string, string> {
  const excluded = new Set(excludedQueryKeys)
  const query = Object.fromEntries(
    Object.entries(route.query).filter(([key, value]) => !excluded.has(key) && value != null),
  )
  const from = router.resolve({ path: route.path, query }).fullPath
  return {
    from,
    ...(window.scrollY > 0 ? { fromScroll: String(window.scrollY) } : {}),
  }
}

export function restoreScrollWhenReady(top: number, router: Router): void {
  if (top <= 0) {
    window.scrollTo({ top: 0, left: 0, behavior: 'auto' })
    return
  }

  const destination = router.currentRoute.value.fullPath
  const finish = () => {
    observer.disconnect()
    window.removeEventListener('resize', check)
    removeGuard()
  }
  const check = () => {
    if (router.currentRoute.value.fullPath !== destination) {
      finish()
    } else if (
      document.documentElement.scrollHeight - document.documentElement.clientHeight >= top
    ) {
      finish()
      window.scrollTo({ top, left: 0, behavior: 'auto' })
    }
  }
  const observer = new MutationObserver(check)
  const removeGuard = router.afterEach(check)
  observer.observe(document.body, { childList: true, subtree: true })
  window.addEventListener('resize', check)
  void nextTick(check)
}

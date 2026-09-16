import type { RouteLocationNormalizedLoaded, Router } from 'vue-router'

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

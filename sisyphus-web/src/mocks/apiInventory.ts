export type ApiEndpoint = `${string} ${string}`

const HTTP_METHODS = new Set(['DELETE', 'GET', 'PATCH', 'POST', 'PUT'])

// 这两组是经评审的对账基线：shared 同时存在于 server OpenAPI 与 demo/test
// MSW；serverOnly 是不由浏览器 mock 消费的 Agent、运维与尚无 UI 的管理面。
// 端点参数统一记作 :param，避免仅参数名不同造成伪漂移，同时仍守住 method 与
// URL 分段结构。新增、删除或改路径时需更新这里，并同步对账文档接受评审。
export const sharedApiEndpoints: readonly ApiEndpoint[] = [
  'DELETE /api/v1/auth/tokens/:param',
  'DELETE /api/v1/projects/:param',
  'DELETE /api/v1/projects/:param/pipelines/:param/builds/:param',
  'DELETE /api/v1/projects/:param/pipelines/:param/builds/:param/artifact-sets/:param',
  'DELETE /api/v1/projects/:param/secrets/:param',
  'DELETE /api/v1/upgrade-packages/:param',
  'DELETE /api/v1/user/pipeline-favorites/:param/:param',
  'GET /api/v1/agents',
  'GET /api/v1/agents/:param',
  'GET /api/v1/artifact-repository',
  'GET /api/v1/artifact-repository/artifacts',
  'GET /api/v1/audit',
  'GET /api/v1/auth/me',
  'GET /api/v1/auth/tokens',
  'GET /api/v1/config/s3',
  'GET /api/v1/log-archives',
  'GET /api/v1/overview',
  'GET /api/v1/pipelines',
  'GET /api/v1/project-deletions',
  'GET /api/v1/projects',
  'GET /api/v1/projects/:param',
  'GET /api/v1/projects/:param/artifact-deletions',
  'GET /api/v1/projects/:param/members',
  'GET /api/v1/projects/:param/pipelines/:param',
  'GET /api/v1/projects/:param/pipelines/:param/builds',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/artifact-sets',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/artifact-sets/:param/file',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/artifacts',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/artifacts/:param',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/jobs/:param/attempts/:param/logs/status',
  'GET /api/v1/projects/:param/pipelines/:param/stats',
  'GET /api/v1/projects/:param/secrets',
  'GET /api/v1/storage/consistency',
  'GET /api/v1/upgrade-packages',
  'GET /api/v1/user/pipeline-favorites',
  'GET /api/v1/users',
  'GET /api/v1/users/directory',
  'PATCH /api/v1/agents/:param',
  'PATCH /api/v1/projects/:param',
  'PATCH /api/v1/users/:param',
  'POST /api/v1/agents',
  'POST /api/v1/agents/:param/cache/delete',
  'POST /api/v1/agents/:param/cache/list',
  'POST /api/v1/agents/:param/upgrade',
  'POST /api/v1/agents/:param/workspace/clean',
  'POST /api/v1/agents/:param/workspace/list',
  'POST /api/v1/agents/upgrade',
  'POST /api/v1/auth/login',
  'POST /api/v1/auth/logout',
  'POST /api/v1/auth/setup',
  'POST /api/v1/auth/tokens',
  'POST /api/v1/config/s3/test-connection',
  'POST /api/v1/log-archives/:param/:param/lost',
  'POST /api/v1/project-deletions/:param/retry',
  'POST /api/v1/projects',
  'POST /api/v1/projects/:param/artifact-deletions/:param/retry',
  'POST /api/v1/projects/:param/pipelines/:param/builds',
  'POST /api/v1/projects/:param/pipelines/:param/builds/:param/cancel',
  'POST /api/v1/projects/:param/pipelines/:param/builds/:param/rerun',
  'POST /api/v1/projects/:param/test-connection',
  'POST /api/v1/projects/scm-branches',
  'POST /api/v1/projects/scm-probe',
  'POST /api/v1/upgrade-packages',
  'POST /api/v1/users',
  'PUT /api/v1/projects/:param/members',
  'PUT /api/v1/projects/:param/pipelines/:param',
  'PUT /api/v1/projects/:param/scm-credential',
  'PUT /api/v1/projects/:param/secrets/:param',
  'PUT /api/v1/user/pipeline-favorites/:param/:param',
  'PUT /api/v1/users/:param/password',
]

export const serverOnlyApiEndpoints: readonly ApiEndpoint[] = [
  'GET /api/v1/agent/artifacts/:param/downloads/:param/:param',
  'GET /api/v1/agent/artifacts/:param/sets/:param/file',
  'GET /api/v1/agent/upgrade-packages/:param',
  'GET /api/v1/config/smtp',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/jobs/:param/attempts/:param/logs',
  'GET /api/v1/projects/:param/pipelines/:param/builds/:param/jobs/:param/attempts/:param/logs/stream',
  'GET /api/v1/projects/:param/pipelines/:param/triggers',
  'GET /api/v1/projects/:param/pipelines/:param/triggers/:param',
  'GET /healthz',
  'PATCH /api/v1/projects/:param/pipelines/:param/triggers/:param',
  'POST /api/v1/agent/artifacts/:param/:param',
  'POST /api/v1/agent/artifacts/:param/:param/complete',
  'POST /api/v1/agent/artifacts/:param/:param/upload-url',
  'POST /api/v1/agent/artifacts/:param/preflight',
  'POST /api/v1/agent/artifacts/:param/sets',
  'POST /api/v1/agent/artifacts/:param/sets/:param/publish',
  'POST /api/v1/agent/register',
  'POST /api/v1/auth/password',
  'POST /api/v1/auth/register',
  'POST /api/v1/projects/:param/pipelines/:param/triggers',
  'PUT /api/v1/config/smtp',
]

export interface EndpointDiff {
  missing: ApiEndpoint[]
  unexpected: ApiEndpoint[]
}

function normalizeEndpoint(method: string, path: string): ApiEndpoint {
  const normalizedMethod = method.toUpperCase()
  if (!HTTP_METHODS.has(normalizedMethod)) {
    throw new Error(`不支持的 HTTP method：${method}`)
  }
  const normalizedPath = path
    .replace(/\{[^/]+\}/g, ':param')
    .replace(/:[^/]+/g, ':param')
  return `${normalizedMethod} ${normalizedPath}`
}

export function collectOpenApiEndpoints(document: unknown): ApiEndpoint[] {
  if (document == null || typeof document !== 'object' || !('paths' in document)) {
    throw new Error('OpenAPI snapshot 缺少 paths')
  }
  const paths = document.paths
  if (paths == null || typeof paths !== 'object') {
    throw new Error('OpenAPI snapshot 的 paths 不是对象')
  }

  const endpoints: ApiEndpoint[] = []
  for (const [path, item] of Object.entries(paths)) {
    if (item == null || typeof item !== 'object') continue
    for (const method of Object.keys(item)) {
      if (HTTP_METHODS.has(method.toUpperCase())) {
        endpoints.push(normalizeEndpoint(method, path))
      }
    }
  }
  return [...new Set(endpoints)].sort()
}

export function collectMockEndpoints(
  handlers: readonly { info: { method: unknown; path: unknown } }[],
): ApiEndpoint[] {
  const endpoints = handlers.map(({ info }) => {
    if (typeof info.method !== 'string' || typeof info.path !== 'string') {
      throw new Error('MSW handler 必须使用字符串 method 与 path 才能参与对账')
    }
    return normalizeEndpoint(info.method, info.path)
  })
  return [...new Set(endpoints)].sort()
}

export function diffEndpoints(
  actual: readonly ApiEndpoint[],
  expected: readonly ApiEndpoint[],
): EndpointDiff {
  const actualSet = new Set(actual)
  const expectedSet = new Set(expected)

  return {
    missing: [...expectedSet].filter((endpoint) => !actualSet.has(endpoint)).sort(),
    unexpected: [...actualSet].filter((endpoint) => !expectedSet.has(endpoint)).sort(),
  }
}

export function formatEndpointDiff(label: string, diff: EndpointDiff): string {
  if (diff.missing.length === 0 && diff.unexpected.length === 0) return ''

  const lines = [`${label} 接口清单漂移：`]
  if (diff.missing.length > 0) {
    lines.push('  缺失（基线有、实际无）：')
    lines.push(...diff.missing.map((endpoint) => `    - ${endpoint}`))
  }
  if (diff.unexpected.length > 0) {
    lines.push('  新增（实际有、基线无）：')
    lines.push(...diff.unexpected.map((endpoint) => `    + ${endpoint}`))
  }
  return lines.join('\n')
}

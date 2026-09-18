import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { describe, expect, it } from 'vitest'

import {
  collectAxumRouterEndpoints,
  collectMockEndpoints,
  collectOpenApiEndpoints,
  diffEndpoints,
  formatEndpointDiff,
  serverOnlyApiEndpoints,
  serverRoutesOutsideOpenApi,
  sharedApiEndpoints,
} from '../apiInventory'
import { createHandlers } from '../handlers'

describe('API inventory reconciliation', () => {
  // api:check 的差异文本是维护者直接消费的命令接口；精确断言保证失败时
  // 能从输出直接定位待补/待删的 method/path，而不是只得到布尔失败。
  it('reports concrete missing and unexpected method/path entries', () => {
    const diff = diffEndpoints(
      ['GET /api/v1/projects', 'POST /api/v1/projects'],
      ['GET /api/v1/projects', 'PATCH /api/v1/projects/:param'],
    )

    expect(formatEndpointDiff('MSW handler', diff)).toBe(
      [
        'MSW handler 接口清单漂移：',
        '  缺失（基线有、实际无）：',
        '    - PATCH /api/v1/projects/:param',
        '  新增（实际有、基线无）：',
        '    + POST /api/v1/projects',
      ].join('\n'),
    )
  })

  it('keeps the Axum, OpenAPI and MSW inventories on the reviewed baseline', () => {
    const snapshotPath = resolve(
      process.cwd(),
      '../sisyphus-server/tests/snapshots/openapi.json',
    )
    const openApi = JSON.parse(readFileSync(snapshotPath, 'utf8')) as unknown
    const routerSource = readFileSync(
      resolve(process.cwd(), '../sisyphus-server/src/api/mod.rs'),
      'utf8',
    )
    const routerEndpoints = collectAxumRouterEndpoints(routerSource)
    const serverEndpoints = collectOpenApiEndpoints(openApi)
    const mockEndpoints = collectMockEndpoints(createHandlers({ authEnforced: false }))
    const routerOnly = new Set(serverRoutesOutsideOpenApi)

    const reports = [
      formatEndpointDiff(
        'server Axum router',
        diffEndpoints(routerEndpoints, [...sharedApiEndpoints, ...serverOnlyApiEndpoints]),
      ),
      formatEndpointDiff(
        'server OpenAPI snapshot',
        diffEndpoints(serverEndpoints, [
          ...sharedApiEndpoints,
          ...serverOnlyApiEndpoints.filter((endpoint) => !routerOnly.has(endpoint)),
        ]),
      ),
      formatEndpointDiff('MSW handler', diffEndpoints(mockEndpoints, sharedApiEndpoints)),
    ].filter(Boolean)
    const report = reports.join('\n\n')

    expect(report, report || undefined).toBe('')
  })
})

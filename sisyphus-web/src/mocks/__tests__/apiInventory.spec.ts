import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { describe, expect, it } from 'vitest'

import {
  collectMockEndpoints,
  collectOpenApiEndpoints,
  diffEndpoints,
  formatEndpointDiff,
  serverOnlyApiEndpoints,
  sharedApiEndpoints,
} from '../apiInventory'
import { createHandlers } from '../handlers'

describe('API inventory reconciliation', () => {
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

  it('keeps the server OpenAPI and MSW handler inventories on the reviewed baseline', () => {
    const snapshotPath = resolve(
      process.cwd(),
      '../sisyphus-server/tests/snapshots/openapi.json',
    )
    const openApi = JSON.parse(readFileSync(snapshotPath, 'utf8')) as unknown
    const serverEndpoints = collectOpenApiEndpoints(openApi)
    const mockEndpoints = collectMockEndpoints(createHandlers({ authEnforced: false }))

    const reports = [
      formatEndpointDiff(
        'server OpenAPI',
        diffEndpoints(serverEndpoints, [...sharedApiEndpoints, ...serverOnlyApiEndpoints]),
      ),
      formatEndpointDiff('MSW handler', diffEndpoints(mockEndpoints, sharedApiEndpoints)),
    ].filter(Boolean)
    const report = reports.join('\n\n')

    expect(report, report || undefined).toBe('')
  })
})

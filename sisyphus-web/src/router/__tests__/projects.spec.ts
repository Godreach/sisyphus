import { describe, expect, it } from 'vitest'

import { router } from '@/router'

describe('项目库路由', () => {
  it('列表为独立集合入口，项目详情和流水线子资源保留各自标题', () => {
    expect(router.resolve('/projects')).toMatchObject({
      name: 'projects',
      meta: { title: 'routes.projects' },
    })
    expect(router.resolve('/projects/demo')).toMatchObject({
      name: 'project-detail',
      meta: { title: 'routes.projectDetail' },
    })
    expect(router.resolve('/projects/demo/pipelines/main/builds/1')).toMatchObject({
      name: 'build-detail',
      meta: { title: 'routes.buildDetail' },
    })
  })
})

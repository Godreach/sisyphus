import { afterEach, describe, expect, it, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'

import { restoreScrollWhenReady } from '@/utils/returnSource'

describe('restoreScrollWhenReady', () => {
  afterEach(() => {
    vi.restoreAllMocks()
    document.body.replaceChildren()
  })

  it('waits for asynchronously rendered height before restoring scroll', async () => {
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/destination', component: { template: '<div />' } }],
    })
    await router.push('/destination')
    await router.isReady()

    let height = 600
    vi.spyOn(document.documentElement, 'scrollHeight', 'get').mockImplementation(() => height)
    vi.spyOn(document.documentElement, 'clientHeight', 'get').mockReturnValue(600)
    const scrollTo = vi.spyOn(window, 'scrollTo').mockImplementation(() => {})

    restoreScrollWhenReady(240, router)
    await Promise.resolve()
    expect(scrollTo).not.toHaveBeenCalled()

    height = 900
    document.body.appendChild(document.createElement('div'))
    await vi.waitFor(() => expect(scrollTo).toHaveBeenCalledWith({
      top: 240,
      left: 0,
      behavior: 'auto',
    }))
  })

  it('cancels pending restoration when navigating elsewhere', async () => {
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/destination', component: { template: '<div />' } },
        { path: '/elsewhere', component: { template: '<div />' } },
      ],
    })
    await router.push('/destination')
    await router.isReady()

    let height = 600
    vi.spyOn(document.documentElement, 'scrollHeight', 'get').mockImplementation(() => height)
    vi.spyOn(document.documentElement, 'clientHeight', 'get').mockReturnValue(600)
    const scrollTo = vi.spyOn(window, 'scrollTo').mockImplementation(() => {})

    restoreScrollWhenReady(240, router)
    await router.push('/elsewhere')
    height = 900
    document.body.appendChild(document.createElement('div'))
    await Promise.resolve()
    expect(scrollTo).not.toHaveBeenCalled()
  })
})

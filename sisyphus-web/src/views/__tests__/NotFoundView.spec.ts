import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, type VueWrapper } from '@vue/test-utils'
import { createPinia, setActivePinia, type Pinia } from 'pinia'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'
import NotFoundView from '@/views/NotFoundView.vue'
import { i18n, setLocale } from '@/i18n'

describe('NotFoundView（404 页面 Naive UI 迁移）', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  beforeEach(async () => {
    setLocale('zh-CN')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/:pathMatch(.*)*', name: 'not-found', component: NotFoundView },
      ],
    })
    await router.push('/')
    await router.isReady()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  function mountView(): VueWrapper {
    return mount(NotFoundView, {
      global: { plugins: [pinia, router, i18n] },
    })
  }

  it('渲染 404 状态标题', () => {
    wrapper = mountView()
    expect(wrapper.text()).toContain('404')
  })

  it('渲染错误描述文本', () => {
    wrapper = mountView()
    expect(wrapper.text()).toContain('抱歉，您访问的页面不存在或已被移除。')
  })

  it('渲染返回首页按钮', () => {
    wrapper = mountView()
    expect(wrapper.text()).toContain('返回首页')
  })

  it('点击返回首页按钮 → 导航到 overview', async () => {
    wrapper = mountView()
    const replaceSpy = vi.spyOn(router, 'replace')

    await wrapper.get('button').trigger('click')

    expect(replaceSpy).toHaveBeenCalledWith({ name: 'overview' })
  })

  it('使用 NResult 组件显示 404 状态', () => {
    wrapper = mountView()
    // NResult 渲染时会包含结果区域
    expect(wrapper.find('.not-found-page').exists()).toBe(true)
  })
})

describe('NotFoundView English locale', () => {
  let pinia: Pinia
  let router: Router
  let wrapper: VueWrapper

  beforeEach(async () => {
    setLocale('en-US')
    pinia = createPinia()
    setActivePinia(pinia)
    router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: '/', name: 'overview', component: { template: '<div />' } },
        { path: '/:pathMatch(.*)*', name: 'not-found', component: NotFoundView },
      ],
    })
    await router.push('/')
    await router.isReady()
  })

  afterEach(() => {
    wrapper?.unmount()
    vi.restoreAllMocks()
  })

  it('英文环境下渲染英文描述和按钮', () => {
    wrapper = mount(NotFoundView, {
      global: { plugins: [pinia, router, i18n] },
    })
    expect(wrapper.text()).toContain('Sorry, the page you visited does not exist or has been removed.')
    expect(wrapper.text()).toContain('Back to home')
  })
})

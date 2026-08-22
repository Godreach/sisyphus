// Vitest 全局测试装配（jsdom 环境，vite.config.ts `test.setupFiles`）。
// jsdom 缺失 window.matchMedia，为 useDarkMode / useBreakpoint 等 composable
// 提供最小替身。默认模拟桌面宽屏（≥768px），非暗色模式。
// 使用 Object.defineProperty 直接赋值，避免被 vi.restoreAllMocks() 还原。
if (!window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
      value: (_query: string) => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  })
}

// jsdom 未实现 Element.prototype.scrollTo（Naive UI NSelect 下拉/内部
// scrollbar 在菜单滚动定位时会调用），补齐最小替身避免未处理异常。
if (typeof window.HTMLElement !== 'undefined' && !window.HTMLElement.prototype.scrollTo) {
  Object.defineProperty(window.HTMLElement.prototype, 'scrollTo', {
    writable: true,
    value: () => {},
  })
}

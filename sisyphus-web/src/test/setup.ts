// Vitest 全局测试装配（jsdom 环境，vite.config.ts `test.setupFiles`）。
// jsdom 缺失 window.matchMedia，为 useDarkMode 等 composable 提供最小替身。
// 使用 Object.defineProperty 直接赋值，避免被 vi.restoreAllMocks() 还原。
if (!window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: query.includes('dark') ? false : true,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  })
}

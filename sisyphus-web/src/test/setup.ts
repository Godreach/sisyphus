// Vitest 全局测试装配（jsdom 环境，vite.config.ts `test.setupFiles`）。
// jsdom 缺失 window.matchMedia，为 useDarkMode / useBreakpoint 等 composable
// 提供最小替身。默认模拟桌面宽屏（≥768px），非暗色模式。
// 使用 Object.defineProperty 直接赋值，避免被 vi.restoreAllMocks() 还原。
if (!window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  })
}

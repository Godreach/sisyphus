// Vitest 全局测试装配（jsdom 环境，vite.config.ts `test.setupFiles`）。
// 骨架期无全局 stub；后续页面票如遇 jsdom 缺失的浏览器 API（如
// IntersectionObserver）在此补全局替身。

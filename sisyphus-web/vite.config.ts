import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'

// sisyphus-web 前端工程（ADR-0003/0020，票 B4-T1）。
//
// - 构建产物进 `dist/`：sisyphus-server 经 rust-embed 内嵌（release 编译期
//   嵌入 / debug 运行时读盘，ADR-0005），server 侧零改动。
// - base 绝对根路径（`/`）：server 在根路径以同源静态面伺服（无子路径挂载
//   概念，config.rs 只有 rest_addr），且 SPA fallback 对任意非 API 路径回
//   index.html——若用相对 base，深链（如 /projects/foo）刷新时浏览器会把
//   `./assets/...` 解析到 /projects/foo/assets/...，被 server 回落成 HTML，
//   资源全挂。绝对 `/` 保证资源永远从根解析。
// - dev server 把 `/api` 请求代理到本机 server（默认 REST 端口 8080，
//   config.rs `DEFAULT_REST_ADDR`）：开发期前后端同源，cookie 会话
//   （SameSite=Lax + HttpOnly）与 CSRF 同源校验在浏览器侧语义一致。
//   swagger-ui 同样转发（debug 构建下 server 暴露）。
export default defineConfig({
  plugins: [vue()],
  base: '/',
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: true,
      },
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['src/test/setup.ts'],
  },
})

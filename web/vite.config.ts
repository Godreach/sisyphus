import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// PROTOTYPE - throwaway. Answers ticket #15: web UI IA + pipeline editor
// interaction shape. Lives only on branch prototype/web-ui-ia, never main.
export default defineConfig({
  plugins: [vue()],
})

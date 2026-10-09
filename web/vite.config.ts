import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 默认 API 基址取 VITE_API_BASE（缺省 /api/v1），便于控制面同进程托管静态资源。
export default defineConfig({
  plugins: [vue()],
  base: process.env.VITE_BASE ?? '/',
  server: {
    port: 5173,
    proxy: {
      '/api/v1': {
        target: process.env.VITE_PROXY_TARGET ?? 'http://127.0.0.1:8080',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    chunkSizeWarningLimit: 2000,
  },
})

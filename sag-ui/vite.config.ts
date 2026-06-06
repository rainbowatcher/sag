import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import vueDevTools from "vite-plugin-vue-devtools"
import tailwindcss from "@tailwindcss/vite"
import { resolve } from 'path'

export default defineConfig({
  plugins: [vue(), tailwindcss(), vueDevTools()],
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:3000', changeOrigin: true },
      '/v1': { target: 'http://127.0.0.1:3000', changeOrigin: true },
      '/health': { target: 'http://127.0.0.1:3000', changeOrigin: true },
    },
  },
})

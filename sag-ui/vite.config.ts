import { resolve } from "path"
import tailwindcss from "@tailwindcss/vite"
import vue from "@vitejs/plugin-vue"
import { defineConfig } from "vite"

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },
  server: {
    port: 5173,
    open: false,
    proxy: {
      "/api": { target: "http://127.0.0.1:37210", changeOrigin: true },
      "/v1": { target: "http://127.0.0.1:37210", changeOrigin: true },
      "/health": { target: "http://127.0.0.1:37210", changeOrigin: true },
    },
  },
})

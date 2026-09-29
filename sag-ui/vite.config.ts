import { readFileSync } from "fs"
import { resolve } from "path"
import tailwindcss from "@tailwindcss/vite"
import vue from "@vitejs/plugin-vue"
import { defineConfig } from "vite"

const pkg = JSON.parse(readFileSync(resolve(__dirname, "package.json"), "utf-8")) as {
  version: string
}

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
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

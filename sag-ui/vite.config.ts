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
  build: {
    rolldownOptions: {
      output: {
        // chart.js 只被 /analytics 路由的 V2TrendChart 使用，单独拆成一个懒加载
        // chunk：`AnalyticsPage` 258 KB → 96 KB，图表库单独 163 KB（gzip 合计不变），
        // 二者各带 content hash —— 改页面不再让 163 KB 的图表库缓存失效
        //（`/assets/**` 是 `immutable` 一年）。
        // `includeDependenciesRecursively: false` 必须显式关：默认 true 会把 vue
        // runtime 一起拖进该组，入口就会静态依赖它，chart.js 反而进首屏 preload。
        // 不用 `output.manualChunks`：rolldown 里它已 deprecated 且不支持对象形式。
        codeSplitting: {
          includeDependenciesRecursively: false,
          groups: [
            {
              name: "vendor-chart",
              test: /[\\/]node_modules[\\/](?:chart\.js|vue-chartjs)[\\/]/,
            },
          ],
        },
      },
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

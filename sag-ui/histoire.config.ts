import { HstVue } from "@histoire/plugin-vue"
import tailwindcss from "@tailwindcss/vite"
import { defineConfig } from "histoire"

export default defineConfig({
  plugins: [HstVue()],
  setupFile: "/src/histoire.setup.ts",
  vite: {
    plugins: [tailwindcss()],
  },
})

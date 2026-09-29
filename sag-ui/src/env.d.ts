/// <reference types="vite/client" />
/// <reference types="@histoire/plugin-vue/components" />

/** Build-time app version injected from package.json via Vite `define`. */
declare const __APP_VERSION__: string

declare module "*.vue" {
  import type { DefineComponent } from "vue"
  const component: DefineComponent<{}, {}, any>
  export default component
}

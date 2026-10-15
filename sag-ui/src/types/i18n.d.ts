import type { t } from "@/lib/i18n"

declare module "vue" {
  interface ComponentCustomProperties {
    $t: typeof t
  }
}

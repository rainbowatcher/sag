<script setup lang="ts">
import { BarChart3, GitBranch, LayoutDashboard, ScrollText, Server, Settings } from "@lucide/vue"
import type { Component } from "vue"
import { computed, watch } from "vue"
import { RouterLink, useRoute } from "vue-router"
import {
  SidebarGroup,
  SidebarGroupContent,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  useSidebar,
} from "@/components/ui/sidebar"
import { t } from "@/lib/i18n"

interface NavItem {
  label: string
  path: string
  icon: Component
}

// Built inside a computed so the labels re-resolve through `t()` whenever the
// active locale changes — a plain array would freeze its strings at setup.
const navItems = computed<NavItem[]>(() => [
  { label: t("nav.dashboard", undefined, "Dashboard"), path: "/", icon: LayoutDashboard },
  { label: t("nav.providers", undefined, "Providers"), path: "/providers", icon: Server },
  { label: t("nav.routes", undefined, "Routes"), path: "/routes", icon: GitBranch },
  { label: t("nav.analytics", undefined, "Analytics"), path: "/analytics", icon: BarChart3 },
  { label: t("nav.logs", undefined, "Logs"), path: "/logs", icon: ScrollText },
  { label: t("nav.settings", undefined, "Settings"), path: "/settings", icon: Settings },
])

const route = useRoute()
const { isMobile, setOpenMobile } = useSidebar()

// On mobile the sidebar renders as a floating Sheet; collapse it once a
// destination is picked so it doesn't keep covering the page it navigated to.
// No-op on desktop, where the sheet isn't used.
function closeMobileSidebar() {
  if (isMobile.value) setOpenMobile(false)
}

// Also collapse on navigations started outside this menu (Disconnect → /connect,
// browser back/forward). Tapping the already-active item produces no route
// change and is handled by the click listener above instead.
watch(() => route.path, closeMobileSidebar)
</script>

<template>
  <SidebarGroup>
    <SidebarGroupContent>
      <SidebarMenu>
        <SidebarMenuItem v-for="item in navItems" :key="item.path">
          <SidebarMenuButton as-child :is-active="route.path === item.path" :tooltip="item.label">
            <RouterLink :to="item.path" @click="closeMobileSidebar">
              <component :is="item.icon" />
              <span>{{ item.label }}</span>
            </RouterLink>
          </SidebarMenuButton>
        </SidebarMenuItem>
      </SidebarMenu>
    </SidebarGroupContent>
  </SidebarGroup>
</template>

<script setup lang="ts">
import {
  BarChart3,
  ChevronRight,
  GitBranch,
  LayoutDashboard,
  ScrollText,
  Server,
  Settings,
} from "@lucide/vue"
import { CollapsibleContent, CollapsibleRoot, CollapsibleTrigger } from "reka-ui"
import type { Component } from "vue"
import { computed, ref, watch } from "vue"
import { RouterLink, useRoute } from "vue-router"
import {
  SidebarGroup,
  SidebarGroupContent,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSub,
  SidebarMenuSubButton,
  SidebarMenuSubItem,
  useSidebar,
} from "@/components/ui/sidebar"
import { t } from "@/lib/i18n"
import { resolveSettingsTab, SETTINGS_TABS } from "@/lib/settingsTabs"

interface NavItem {
  label: string
  path: string
  icon: Component
}

// Built inside a computed so the labels re-resolve through `t()` whenever the
// active locale changes — a plain array would freeze its strings at setup.
// Settings is rendered separately below as an expandable group.
const navItems = computed<NavItem[]>(() => [
  { label: t("nav.dashboard"), path: "/", icon: LayoutDashboard },
  { label: t("nav.providers"), path: "/providers", icon: Server },
  { label: t("nav.routes"), path: "/routes", icon: GitBranch },
  { label: t("nav.analytics"), path: "/analytics", icon: BarChart3 },
  { label: t("nav.logs"), path: "/logs", icon: ScrollText },
])

const route = useRoute()
const { isMobile, state, setOpenMobile } = useSidebar()

const isSettingsRoute = computed(() => route.name === "settings")

// Sub-item labels also re-resolve through `t()` on locale change.
const settingsTabs = computed(() =>
  SETTINGS_TABS.map((tab) => ({ value: tab.value, label: t(tab.key) })),
)

// Highlight the sub-item matching `?tab=`; a bare `/settings` highlights the
// default tab (`network`), mirroring what SettingsPage actually renders.
const activeSettingsTab = computed(() =>
  isSettingsRoute.value ? resolveSettingsTab(route.query.tab) : null,
)

// The icon rail has no room for sub-items, so there the parent is a plain link
// to `/settings`. Mobile keeps the expandable list: its Sheet is wide enough.
const isCollapsed = computed(() => !isMobile.value && state.value === "collapsed")

const settingsOpen = ref(isSettingsRoute.value)

// Auto-expand when navigating into settings; never force-collapse, so a manual
// toggle made elsewhere survives until the user changes it again.
watch(isSettingsRoute, (onSettings) => {
  if (onSettings) settingsOpen.value = true
})

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

        <SidebarMenuItem>
          <!-- Icon rail: no room for sub-items, so the parent links straight to settings. -->
          <SidebarMenuButton
            v-if="isCollapsed"
            as-child
            :is-active="isSettingsRoute"
            :tooltip="t('nav.settings')"
          >
            <RouterLink to="/settings" @click="closeMobileSidebar">
              <Settings />
              <span>{{ t("nav.settings") }}</span>
            </RouterLink>
          </SidebarMenuButton>

          <CollapsibleRoot v-else v-model:open="settingsOpen" class="group/collapsible">
            <CollapsibleTrigger as-child>
              <SidebarMenuButton :is-active="isSettingsRoute">
                <Settings />
                <span>{{ t("nav.settings") }}</span>
                <ChevronRight
                  class="ml-auto transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90"
                />
              </SidebarMenuButton>
            </CollapsibleTrigger>
            <CollapsibleContent>
              <SidebarMenuSub>
                <SidebarMenuSubItem v-for="tab in settingsTabs" :key="tab.value">
                  <SidebarMenuSubButton as-child :is-active="activeSettingsTab === tab.value">
                    <RouterLink
                      :to="{ path: '/settings', query: { tab: tab.value } }"
                      @click="closeMobileSidebar"
                    >
                      <span>{{ tab.label }}</span>
                    </RouterLink>
                  </SidebarMenuSubButton>
                </SidebarMenuSubItem>
              </SidebarMenuSub>
            </CollapsibleContent>
          </CollapsibleRoot>
        </SidebarMenuItem>
      </SidebarMenu>
    </SidebarGroupContent>
  </SidebarGroup>
</template>

<script setup lang="ts">
import {
  LayoutDashboard,
  Server,
  GitBranch,
  BarChart3,
  ScrollText,
  Settings,
  Wifi,
  WifiOff,
  Moon,
  Sun,
  LogOut,
} from "@lucide/vue"
import { useMediaQuery } from "@vueuse/core"
import { computed, ref, watch } from "vue"
import { RouterLink, RouterView, useRoute, useRouter } from "vue-router"
import { Badge } from "@/components/ui/badge"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarSeparator,
  SidebarTrigger,
} from "@/components/ui/sidebar"
import { isDark, toggleDark } from "@/composables/useTheme"
import { cn } from "@/lib/utils"
import { useConnectionStore } from "@/stores/connection"
import { useHealthStore } from "@/stores/health"
import Button from "./ui/button/Button.vue"

const store = useConnectionStore()
const health = useHealthStore()
const route = useRoute()
const router = useRouter()

function handleDisconnect() {
  store.disconnect()
  router.replace("/connect")
}

// Three-tier sidebar: desktop (≥1280px) expanded, tablet icon rail, mobile floating Sheet.
// Mobile breakpoint matches SidebarProvider's internal useMediaQuery("(max-width: 768px)").
const isMobile = useMediaQuery("(max-width: 768px)")
const isDesktop = useMediaQuery("(min-width: 1280px)")

const SIDEBAR_OPEN_KEY = "sag-sidebar-open"

function loadSidebarOpen(): boolean | undefined {
  try {
    const raw = localStorage.getItem(SIDEBAR_OPEN_KEY)
    return raw === null ? undefined : raw === "1"
  } catch {
    return undefined
  }
}

function saveSidebarOpen(open: boolean) {
  try {
    localStorage.setItem(SIDEBAR_OPEN_KEY, open ? "1" : "0")
  } catch {
    // ignore quota / private mode
  }
}

const storedSidebarOpen = loadSidebarOpen()
const sidebarOpen = ref(storedSidebarOpen ?? true)

let tierBootstrapped = false
watch(
  [isMobile, isDesktop],
  () => {
    if (isMobile.value) return
    if (!tierBootstrapped) {
      tierBootstrapped = true
      if (storedSidebarOpen === undefined) {
        sidebarOpen.value = isDesktop.value
        saveSidebarOpen(sidebarOpen.value)
      }
      return
    }
    sidebarOpen.value = isDesktop.value
    saveSidebarOpen(sidebarOpen.value)
  },
  { immediate: true },
)

watch(sidebarOpen, (open) => {
  if (!isMobile.value) saveSidebarOpen(open)
})

// Sidebar collapses 16rem → 3rem; widen content by the reclaimed 13rem so
// centered max-width columns don't grow side gutters on wide screens.
// Collapsed caps = expanded caps + 13rem (60→73, 80→93, 90→103).
const contentMaxClass = computed(() =>
  sidebarOpen.value
    ? "max-w-6xl xl:max-w-7xl 2xl:max-w-[90rem]"
    : "max-w-[73rem] xl:max-w-[93rem] 2xl:max-w-[103rem]",
)

interface NavItem {
  label: string
  path: string
  icon: any
}

const navItems: NavItem[] = [
  { label: "Dashboard", path: "/", icon: LayoutDashboard },
  { label: "Providers", path: "/providers", icon: Server },
  { label: "Routes", path: "/routes", icon: GitBranch },
  { label: "Analytics", path: "/analytics", icon: BarChart3 },
  { label: "Logs", path: "/logs", icon: ScrollText },
  { label: "Settings", path: "/settings", icon: Settings },
]
</script>

<template>
  <SidebarProvider v-model:open="sidebarOpen" class="h-svh overflow-hidden">
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" tooltip="SAG Gateway">
              <div
                class="bg-sidebar-primary text-sidebar-primary-foreground flex aspect-square size-8 items-center justify-center rounded-md text-xs font-bold"
              >
                S
              </div>
              <div class="grid flex-1 text-left text-sm leading-tight">
                <span class="truncate font-semibold">SAG Gateway</span>
                <span class="text-muted-foreground truncate text-xs">
                  v{{ health.gatewayVersion || "-" }}
                </span>
              </div>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              <SidebarMenuItem v-for="item in navItems" :key="item.path">
                <SidebarMenuButton
                  as-child
                  :is-active="route.path === item.path"
                  :tooltip="item.label"
                >
                  <RouterLink :to="item.path">
                    <component :is="item.icon" />
                    <span>{{ item.label }}</span>
                  </RouterLink>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter>
        <SidebarSeparator class="mx-0 w-full" />
        <div
          class="flex items-center justify-between gap-2 px-1 group-data-[collapsible=icon]:justify-center group-data-[collapsible=icon]:px-0"
        >
          <Badge
            :variant="health.gatewayOnline ? 'secondary' : 'outline'"
            class="gap-1.5 text-[10px] group-data-[collapsible=icon]:hidden"
            :class="health.gatewayOnline ? '' : 'text-destructive border-destructive/30'"
          >
            <component :is="health.gatewayOnline ? Wifi : WifiOff" class="h-3 w-3" />
            {{ health.gatewayOnline ? "Connected" : "Offline" }}
          </Badge>
          <Button
            variant="ghost"
            size="icon-sm"
            class="hover:text-foreground transition-colors"
            @click="() => toggleDark()"
          >
            <component :is="isDark ? Sun : Moon" class="h-3.5 w-3.5" />
          </Button>
        </div>
        <p
          v-if="store.gatewayUrl"
          class="text-muted-foreground truncate px-1 font-mono text-[10px] group-data-[collapsible=icon]:hidden"
        >
          {{ store.gatewayUrl }}
        </p>
        <Button
          variant="outline"
          class="text-muted-foreground hover:text-destructive w-full gap-1.5 text-[11px] transition-colors group-data-[collapsible=icon]:size-8 group-data-[collapsible=icon]:self-center group-data-[collapsible=icon]:p-0"
          @click="handleDisconnect"
        >
          <LogOut class="h-3 w-3" />
          <span class="group-data-[collapsible=icon]:hidden">Disconnect</span>
        </Button>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>

    <SidebarInset class="min-h-0">
      <header v-if="isMobile" class="flex h-12 shrink-0 items-center gap-2 border-b px-4">
        <SidebarTrigger class="-ml-1" />
        <span class="text-sm font-semibold">SAG Gateway</span>
      </header>
      <div class="min-h-0 flex-1 overflow-hidden">
        <div :class="cn('mx-auto h-full p-6 xl:p-8 2xl:p-10', contentMaxClass)">
          <RouterView />
        </div>
      </div>
    </SidebarInset>
  </SidebarProvider>
</template>

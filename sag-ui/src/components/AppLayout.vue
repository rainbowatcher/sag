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
import { RouterView, useRoute, useRouter } from "vue-router"
import { Badge } from "@/components/ui/badge"
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
  <div class="bg-muted/30 flex h-screen">
    <aside class="bg-sidebar flex w-60 flex-col border-r xl:w-64 2xl:w-72">
      <div class="flex items-center gap-2 border-b px-5 py-4">
        <div
          class="bg-primary text-primary-foreground flex h-7 w-7 items-center justify-center rounded text-xs font-bold"
        >
          S
        </div>
        <div>
          <h1 class="text-sm font-semibold">SAG Gateway</h1>
          <p class="text-muted-foreground text-[10px]">v{{ health.gatewayVersion || "-" }}</p>
        </div>
      </div>

      <nav class="flex-1 space-y-1 px-2 py-2">
        <Button
          variant="ghost"
          v-for="item in navItems"
          :key="item.path"
          @click="router.push(item.path)"
          :class="
            cn(
              'flex w-full items-center justify-start gap-3 rounded-md px-3 py-2 text-sm transition-colors',
              route.path === item.path
                ? 'bg-primary/10 text-foreground font-medium'
                : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground',
            )
          "
        >
          <component :is="item.icon" class="h-4 w-4" />
          {{ item.label }}
        </Button>
      </nav>

      <div class="space-y-2.5 border-t px-5 py-3">
        <div class="text-muted-foreground flex items-center justify-between text-xs">
          <Badge
            :variant="health.gatewayOnline ? 'secondary' : 'outline'"
            class="gap-1.5 text-[10px]"
            :class="health.gatewayOnline ? '' : 'text-destructive border-destructive/30'"
          >
            <component :is="health.gatewayOnline ? Wifi : WifiOff" class="h-3 w-3" />
            {{ health.gatewayOnline ? "Connected" : "Offline" }}
          </Badge>
          <Button
            variant="ghost"
            size="icon-sm"
            @click="() => toggleDark()"
            class="hover:text-foreground transition-colors"
          >
            <component :is="isDark ? Sun : Moon" class="h-3.5 w-3.5" />
          </Button>
        </div>
        <p v-if="store.gatewayUrl" class="text-muted-foreground truncate font-mono text-[10px]">
          {{ store.gatewayUrl }}
        </p>
        <Button
          variant="outline"
          @click="handleDisconnect"
          class="text-muted-foreground hover:text-destructive flex w-full items-center gap-1.5 text-[11px] transition-colors"
        >
          <LogOut class="h-3 w-3" />
          Disconnect
        </Button>
      </div>
    </aside>

    <main class="flex-1 overflow-hidden">
      <div class="mx-auto h-full max-w-6xl p-6 xl:max-w-7xl xl:p-8 2xl:max-w-[90rem] 2xl:p-10">
        <RouterView />
      </div>
    </main>
  </div>
</template>

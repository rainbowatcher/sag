<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { RouterView, useRoute, useRouter } from 'vue-router'
import { useApiStore } from '@/stores/api'
import { cn } from '@/lib/utils'
import {
  LayoutDashboard, Server, GitBranch, BarChart3,
  Wifi, WifiOff, Moon, Sun, LogOut
} from 'lucide-vue-next'

const store = useApiStore()
const route = useRoute()
const router = useRouter()
const isDark = ref(false)

function toggleDark() {
  isDark.value = !isDark.value
  document.documentElement.classList.toggle('dark', isDark.value)
}

function handleDisconnect() {
  store.disconnect()
  router.replace('/connect')
}

onMounted(() => {
  store.checkHealth()
})

interface NavItem {
  label: string
  path: string
  icon: any
}

const navItems: NavItem[] = [
  { label: 'Dashboard', path: '/', icon: LayoutDashboard },
  { label: 'Providers', path: '/providers', icon: Server },
  { label: 'Routes', path: '/routes', icon: GitBranch },
  { label: 'Analytics', path: '/analytics', icon: BarChart3 },
]
</script>

<template>
  <div class="flex h-screen bg-muted/30">
    <!-- Sidebar -->
    <aside class="w-60 border-r bg-card flex flex-col">
      <div class="px-5 py-4 border-b flex items-center gap-2">
        <div class="h-7 w-7 rounded bg-primary flex items-center justify-center text-primary-foreground text-xs font-bold">S</div>
        <div>
          <h1 class="text-sm font-semibold">SAG Gateway</h1>
          <p class="text-[10px] text-muted-foreground">v{{ store.gatewayVersion || '-' }}</p>
        </div>
      </div>

      <nav class="flex-1 py-2 px-2 space-y-1">
        <button
          v-for="item in navItems"
          :key="item.path"
          @click="router.push(item.path)"
          :class="cn(
            'flex items-center gap-3 w-full px-3 py-2 text-sm rounded-md transition-colors',
            route.path === item.path
              ? 'bg-primary/10 text-primary font-medium'
              : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground',
          )"
        >
          <component :is="item.icon" class="h-4 w-4" />
          {{ item.label }}
        </button>
      </nav>

      <!-- Connection status -->
      <div class="border-t px-5 py-3 space-y-2.5">
        <div class="flex items-center justify-between text-xs text-muted-foreground">
          <div class="flex items-center gap-1.5">
            <component :is="store.gatewayOnline ? Wifi : WifiOff" class="h-3.5 w-3.5" :class="store.gatewayOnline ? 'text-green-500' : 'text-red-500'" />
            <span>{{ store.gatewayOnline ? 'Connected' : 'Offline' }}</span>
          </div>
          <button @click="toggleDark" class="hover:text-foreground transition-colors">
            <component :is="isDark ? Sun : Moon" class="h-3.5 w-3.5" />
          </button>
        </div>
        <p v-if="store.gatewayUrl" class="text-[10px] text-muted-foreground truncate font-mono" :title="store.gatewayUrl">
          {{ store.gatewayUrl }}
        </p>
        <button
          @click="handleDisconnect"
          class="flex items-center gap-1.5 w-full text-[11px] text-muted-foreground hover:text-destructive transition-colors"
        >
          <LogOut class="h-3 w-3" />
          Disconnect
        </button>
      </div>
    </aside>

    <!-- Main content -->
    <main class="flex-1 overflow-y-auto">
      <div class="max-w-6xl mx-auto p-6">
        <RouterView />
      </div>
    </main>
  </div>
</template>

<script setup lang="ts">
import { GitBranch, HeartPulse, Server, ShieldAlert } from "@lucide/vue"
import { computed, onMounted } from "vue"
import CircuitBreakerPanel from "@/components/circuit/CircuitBreakerPanel.vue"
import { Card, CardContent } from "@/components/ui/card"
import { t } from "@/lib/i18n"
import { useAnalyticsStore } from "@/stores/analytics"
import { useCircuitBreakerStore } from "@/stores/circuitBreaker"
import { useHealthStore } from "@/stores/health"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"

const providerStore = useProviderStore()
const routeStore = useRouteStore()
const analyticsStore = useAnalyticsStore()
const healthStore = useHealthStore()
const circuitBreakerStore = useCircuitBreakerStore()

onMounted(async () => {
  await Promise.all([
    providerStore.fetchProviders(),
    routeStore.fetchRoutes(),
    analyticsStore.fetchStats(),
    healthStore.checkHealth(),
    circuitBreakerStore.fetchBreakers(),
  ])
})

// Dashboard is the ops cockpit: configuration context (provider/route
// counts) + real-time operational state (circuit breakers, gateway health).
// Aggregated analytics (calls, errors, trends) live in Analytics > Insights.
const cards = computed(() => [
  {
    label: t("nav.providers"),
    value: analyticsStore.summary.providerCount,
    icon: Server,
    color: "text-blue-500",
  },
  {
    label: t("nav.routes"),
    value: analyticsStore.summary.routeCount,
    icon: GitBranch,
    color: "text-green-500",
  },
  {
    label: t("dashboard.circuitsOpen"),
    value: circuitBreakerStore.openBreakers.length,
    icon: ShieldAlert,
    color: circuitBreakerStore.openBreakers.length > 0 ? "text-red-500" : "text-muted-foreground",
  },
  {
    label: t("dashboard.gateway"),
    value: healthStore.gatewayOnline ? t("common.online") : t("common.offline"),
    icon: HeartPulse,
    color: healthStore.gatewayOnline ? "text-green-500" : "text-red-500",
  },
])
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0 space-y-4">
      <!-- Stat cards -->
      <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Card v-for="card in cards" :key="card.label" class="transition-shadow hover:shadow-md">
          <CardContent class="flex items-center gap-4 p-4 xl:gap-5">
            <div class="bg-muted flex h-10 w-10 items-center justify-center rounded-lg">
              <component :is="card.icon" class="h-5 w-5" :class="card.color" />
            </div>
            <div>
              <p class="text-muted-foreground text-xs">{{ card.label }}</p>
              <p class="text-xl font-bold xl:text-2xl">{{ card.value }}</p>
            </div>
          </CardContent>
        </Card>
      </div>

      <!-- Circuit breaker panel -->
      <CircuitBreakerPanel />
    </div>
  </div>
</template>

<script setup lang="ts">
import { Server, GitBranch, MessageSquare, AlertTriangle, AlertCircle, Loader2 } from "@lucide/vue"
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  ArcElement,
  Title,
  Tooltip,
  Legend,
  Filler,
} from "chart.js"
import { ref, onMounted, computed, watch } from "vue"
import { Line, Pie } from "vue-chartjs"
import CircuitBreakerPanel from "@/components/circuit/CircuitBreakerPanel.vue"
import RefreshButton from "@/components/RefreshButton.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { useChartTheme } from "@/composables/useChartTheme"
import { useAnalyticsStore } from "@/stores/analytics"
import { useCircuitBreakerStore } from "@/stores/circuitBreaker"
import { useHealthStore } from "@/stores/health"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"

ChartJS.register(
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  ArcElement,
  Title,
  Tooltip,
  Legend,
  Filler,
)

const providerStore = useProviderStore()
const routeStore = useRouteStore()
const analyticsStore = useAnalyticsStore()
const healthStore = useHealthStore()
const circuitBreakerStore = useCircuitBreakerStore()
const { chartColors } = useChartTheme()

onMounted(async () => {
  await Promise.all([
    providerStore.fetchProviders(),
    routeStore.fetchRoutes(),
    analyticsStore.fetchStats(),
    healthStore.checkHealth(),
    circuitBreakerStore.fetchBreakers(),
  ])
})

const cards = [
  {
    label: "Providers",
    value: () => analyticsStore.summary.providerCount,
    icon: Server,
    color: "text-blue-500",
  },
  {
    label: "Routes",
    value: () => analyticsStore.summary.routeCount,
    icon: GitBranch,
    color: "text-green-500",
  },
  {
    label: "Total Calls",
    value: () => analyticsStore.summary.totalCalls,
    icon: MessageSquare,
    color: "text-purple-500",
  },
  {
    label: "Errors",
    value: () => analyticsStore.summary.totalErrors,
    icon: AlertTriangle,
    color: analyticsStore.summary.totalErrors > 0 ? "text-red-500" : "text-muted-foreground",
  },
]

// ── Trend / distribution charts ───────────────────────────────────
type TimeRange = "1h" | "24h" | "7d"
const dashRange = ref<TimeRange>("24h")
const dashProvider = ref("")

const rangeMs: Record<TimeRange, number> = {
  "1h": 3600_000,
  "24h": 86_400_000,
  "7d": 604_800_000,
}
const rangeBucket: Record<TimeRange, number> = {
  "1h": 300, // 5 min
  "24h": 3600, // 1 hour
  "7d": 14400, // 4 hours
}

async function refreshDashboard() {
  const now = Date.now()
  const fromMs = now - rangeMs[dashRange.value]
  await Promise.all([
    analyticsStore.fetchTrend(
      rangeBucket[dashRange.value],
      fromMs,
      now,
      dashProvider.value || undefined,
    ),
    analyticsStore.fetchDistribution(dashProvider.value || undefined),
  ])
}

// Re-fetch when time range or provider filter changes, and on mount.
watch([dashRange, dashProvider], () => refreshDashboard(), { immediate: true })

const bucketLabels = computed(() =>
  analyticsStore.trend.map((r) =>
    new Date(r.bucket).toLocaleString(
      "en-CA",
      (dashRange.value === "7d"
        ? { month: "short", day: "numeric", hour: "2-digit" }
        : { hour: "2-digit", minute: "2-digit" }) as Intl.DateTimeFormatOptions,
    ),
  ),
)

const trendTokensChart = computed(() => {
  const [prompt, completion] = [chartColors.value[0], chartColors.value[1]]
  return {
    labels: bucketLabels.value,
    datasets: [
      {
        label: "Prompt",
        data: analyticsStore.trend.map((r) => r.prompt_tokens),
        borderColor: prompt,
        backgroundColor: prompt.replace("rgb(", "rgba(").replace(")", ",0.1)"),
        fill: true,
        tension: 0.3,
        pointRadius: 0,
      },
      {
        label: "Completion",
        data: analyticsStore.trend.map((r) => r.completion_tokens),
        borderColor: completion,
        backgroundColor: completion.replace("rgb(", "rgba(").replace(")", ",0.1)"),
        fill: true,
        tension: 0.3,
        pointRadius: 0,
      },
    ],
  }
})

const trendCallsChart = computed(() => {
  const [calls, success] = [chartColors.value[2], chartColors.value[1]]
  return {
    labels: bucketLabels.value,
    datasets: [
      {
        label: "Calls",
        data: analyticsStore.trend.map((r) => r.call_count),
        borderColor: calls,
        backgroundColor: calls.replace("rgb(", "rgba(").replace(")", ",0.1)"),
        fill: true,
        tension: 0.3,
        pointRadius: 0,
      },
      {
        label: "Success",
        data: analyticsStore.trend.map((r) => r.success_count),
        borderColor: success,
        backgroundColor: "transparent",
        tension: 0.3,
        pointRadius: 0,
      },
    ],
  }
})

const pieChart = computed(() => ({
  labels: analyticsStore.distribution.map((r) => `${r.provider}/${r.model}`),
  datasets: [
    {
      data: analyticsStore.distribution.map((r) => r.call_count),
      backgroundColor: analyticsStore.distribution.map(
        (_, i) => chartColors.value[i % chartColors.value.length],
      ),
    },
  ],
}))
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
              <p class="text-xl font-bold xl:text-2xl">{{ card.value() }}</p>
            </div>
          </CardContent>
        </Card>
      </div>

      <!-- Circuit breaker panel -->
      <CircuitBreakerPanel />

      <!-- Trend & distribution charts -->
      <div class="flex flex-wrap items-center gap-3">
        <div class="bg-muted inline-flex rounded-md p-0.5">
          <button
            v-for="r in ['1h', '24h', '7d'] as TimeRange[]"
            :key="r"
            @click="dashRange = r"
            class="rounded-sm px-3 py-1 text-sm transition-colors"
            :class="
              dashRange === r ? 'bg-background font-medium shadow-sm' : 'text-muted-foreground'
            "
          >
            {{ r }}
          </button>
        </div>
        <Input v-model="dashProvider" class="w-40 xl:w-52" placeholder="provider" />
        <RefreshButton
          :loading="analyticsStore.trendLoading || analyticsStore.distLoading"
          size="default"
          class="ml-auto"
          @click="refreshDashboard()"
        />
      </div>

      <Alert v-if="analyticsStore.trendError || analyticsStore.distError" variant="destructive">
        <AlertCircle class="h-4 w-4" />
        <AlertTitle>Failed to load dashboard data</AlertTitle>
        <AlertDescription class="break-words">
          {{ analyticsStore.trendError || analyticsStore.distError }}
        </AlertDescription>
      </Alert>
    </div>

    <div class="min-h-0 flex-1 pt-4">
      <div class="h-full overflow-y-auto">
        <div class="grid grid-cols-1 gap-6 lg:grid-cols-2 xl:grid-cols-3">
          <!-- Distribution pie chart -->
          <Card>
            <CardHeader>
              <CardTitle class="text-sm font-medium">Call Distribution</CardTitle>
            </CardHeader>
            <CardContent>
              <div
                v-if="analyticsStore.distLoading"
                class="text-muted-foreground flex items-center justify-center py-16"
              >
                <Loader2 class="h-5 w-5 animate-spin" />
              </div>
              <div
                v-else-if="analyticsStore.distribution.length === 0"
                class="text-muted-foreground flex items-center justify-center py-16 text-sm"
              >
                No data available.
              </div>
              <div v-else class="mx-auto max-w-xs xl:max-w-sm">
                <Pie
                  :data="pieChart"
                  :options="{ plugins: { legend: { position: 'bottom' as const } } }"
                />
              </div>
            </CardContent>
          </Card>

          <!-- Token trend line chart -->
          <Card>
            <CardHeader>
              <CardTitle class="text-sm font-medium">Token Trend</CardTitle>
            </CardHeader>
            <CardContent>
              <div
                v-if="analyticsStore.trendLoading"
                class="text-muted-foreground flex items-center justify-center py-16"
              >
                <Loader2 class="h-5 w-5 animate-spin" />
              </div>
              <div
                v-else-if="analyticsStore.trend.length === 0"
                class="text-muted-foreground flex items-center justify-center py-16 text-sm"
              >
                No data available.
              </div>
              <Line
                v-else
                :data="trendTokensChart"
                :options="{
                  responsive: true,
                  scales: { y: { beginAtZero: true } },
                  plugins: { legend: { position: 'bottom' as const } },
                }"
              />
            </CardContent>
          </Card>

          <!-- Call count trend line chart -->
          <Card class="lg:col-span-2 xl:col-span-1">
            <CardHeader>
              <CardTitle class="text-sm font-medium">Call Count Trend</CardTitle>
            </CardHeader>
            <CardContent>
              <div
                v-if="analyticsStore.trendLoading"
                class="text-muted-foreground flex items-center justify-center py-16"
              >
                <Loader2 class="h-5 w-5 animate-spin" />
              </div>
              <div
                v-else-if="analyticsStore.trend.length === 0"
                class="text-muted-foreground flex items-center justify-center py-16 text-sm"
              >
                No data available.
              </div>
              <Line
                v-else
                :data="trendCallsChart"
                :options="{
                  responsive: true,
                  scales: { y: { beginAtZero: true } },
                  plugins: { legend: { position: 'bottom' as const } },
                }"
              />
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  </div>
</template>

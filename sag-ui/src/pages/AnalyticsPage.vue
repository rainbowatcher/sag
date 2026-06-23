<script setup lang="ts">
import {
  AlertCircle,
  Search,
  RefreshCw,
  Loader2,
  TrendingUp,
  Zap,
  Clock,
  ChevronLeft,
  ChevronRight,
} from "@lucide/vue"
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
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs"
import { formatDuration } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"
import type { CallLog } from "@/types/api"

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

const store = useAnalyticsStore()

// ── Dashboard tab state ────────────────────────────────────────────
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
    store.fetchTrend(rangeBucket[dashRange.value], fromMs, now, dashProvider.value || undefined),
    store.fetchDistribution(dashProvider.value || undefined),
  ])
}

// Re-fetch when time range or provider filter changes, and on mount.
watch([dashRange, dashProvider], () => refreshDashboard(), { immediate: true })

const trendTokensChart = computed(() => ({
  labels: store.trend.map((r) =>
    new Date(r.bucket).toLocaleString(
      dashRange.value === "7d" ? "en-CA" : "en-CA",
      (dashRange.value === "7d"
        ? { month: "short", day: "numeric", hour: "2-digit" }
        : { hour: "2-digit", minute: "2-digit" }) as Intl.DateTimeFormatOptions,
    ),
  ),
  datasets: [
    {
      label: "Prompt",
      data: store.trend.map((r) => r.prompt_tokens),
      borderColor: "rgb(59,130,246)",
      backgroundColor: "rgba(59,130,246,0.1)",
      fill: true,
      tension: 0.3,
      pointRadius: 0,
    },
    {
      label: "Completion",
      data: store.trend.map((r) => r.completion_tokens),
      borderColor: "rgb(34,197,94)",
      backgroundColor: "rgba(34,197,94,0.1)",
      fill: true,
      tension: 0.3,
      pointRadius: 0,
    },
  ],
}))

const trendCallsChart = computed(() => ({
  labels: store.trend.map((r) =>
    new Date(r.bucket).toLocaleString(
      "en-CA",
      (dashRange.value === "7d"
        ? { month: "short", day: "numeric", hour: "2-digit" }
        : { hour: "2-digit", minute: "2-digit" }) as Intl.DateTimeFormatOptions,
    ),
  ),
  datasets: [
    {
      label: "Calls",
      data: store.trend.map((r) => r.call_count),
      borderColor: "rgb(168,85,247)",
      backgroundColor: "rgba(168,85,247,0.1)",
      fill: true,
      tension: 0.3,
      pointRadius: 0,
    },
    {
      label: "Success",
      data: store.trend.map((r) => r.success_count),
      borderColor: "rgb(34,197,94)",
      backgroundColor: "transparent",
      tension: 0.3,
      pointRadius: 0,
    },
  ],
}))

const pieChart = computed(() => ({
  labels: store.distribution.map((r) => `${r.provider}/${r.model}`),
  datasets: [
    {
      data: store.distribution.map((r) => r.call_count),
      backgroundColor: [
        "rgb(59,130,246)",
        "rgb(34,197,94)",
        "rgb(168,85,247)",
        "rgb(251,146,60)",
        "rgb(236,72,153)",
        "rgb(14,165,233)",
        "rgb(251,191,36)",
        "rgb(99,102,241)",
      ],
    },
  ],
}))

// ── Cache tab state ────────────────────────────────────────────────
const cacheFilterProvider = ref("")
const cacheFilterModel = ref("")

async function refreshCache() {
  await store.fetchCacheStats(
    cacheFilterProvider.value || undefined,
    cacheFilterModel.value || undefined,
  )
}

// ── Stats tab state ────────────────────────────────────────────────
const statsFilterProvider = ref("")
const statsFilterModel = ref("")

onMounted(() => {
  store.fetchStats()
  store.fetchLogs()
})

async function refreshStats() {
  await store.fetchStats(
    statsFilterProvider.value || undefined,
    statsFilterModel.value || undefined,
  )
}

const statsTotals = computed(() => {
  let calls = 0
  let tokens = 0
  let errors = 0
  let weightedMs = 0
  for (const r of store.stats) {
    calls += r.call_count
    tokens += r.total_tokens
    errors += r.error_count
    weightedMs += r.avg_duration_ms * r.call_count
  }
  return {
    calls,
    tokens,
    avgMs: calls > 0 ? weightedMs / calls : 0,
    errors,
  }
})

// ── Logs tab state ─────────────────────────────────────────────────
const logsFilterProvider = ref("")
const logsFilterModel = ref("")
const logsFilterRealModel = ref("")
const logsFilterStatusCode = ref("")

async function refreshLogs() {
  store.resetLogsPage()
  // The browser <input type="number"> only validates manual key entry —
  // pasted text like "abc" slips through and would yield `Number("abc") →
  // NaN`, which `String(NaN) → "NaN"` would then fail Option<u16>
  // deserialization in the backend with HTTP 400. Reject on the client
  // and silently drop the filter (treat as "no status filter") instead of
  // forwarding an invalid value.
  let status_code: number | undefined
  const trim = logsFilterStatusCode.value.trim()
  if (trim.length > 0) {
    const n = Number(trim)
    if (Number.isFinite(n) && Number.isInteger(n) && n >= 0 && n <= 65535) {
      status_code = n
    }
  }
  await store.fetchLogs({
    provider: logsFilterProvider.value || undefined,
    model: logsFilterModel.value || undefined,
    real_model: logsFilterRealModel.value || undefined,
    status_code,
  })
}

const logsCurrentPage = computed(() => Math.floor(store.logsOffset / store.logsLimit) + 1)

function formatTs(iso: string): string {
  if (!iso) return ""
  try {
    return new Date(iso).toLocaleString("en-CA", { hour12: false }).replace(",", "")
  } catch {
    return iso
  }
}

/** Tokens/sec over the generation window after TTFT; null when inputs are missing or invalid. */
function tpsFor(row: CallLog): number | null {
  const usage = row.usage
  const ttft = row.ttft_ms
  const upstream = row.upstream_duration_ms
  if (!usage || ttft == null || upstream == null) return null
  const genMs = upstream - ttft
  if (genMs <= 0) return null
  return (usage.completion_tokens * 1000) / genMs
}

function fmtTps(row: CallLog): string | null {
  const tps = tpsFor(row)
  return tps == null ? null : `${tps.toFixed(1)} tps`
}
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Analytics</h2>
        <p class="text-muted-foreground mt-1 text-sm">调用统计 / Token 消耗 / 请求记录</p>
      </div>
    </div>

    <Tabs :default-value="'stats'" class="space-y-6">
      <TabsList>
        <TabsTrigger value="stats">Stats</TabsTrigger>
        <TabsTrigger value="dashboard">Dashboard</TabsTrigger>
        <TabsTrigger value="logs">Logs</TabsTrigger>
        <TabsTrigger value="cache">Cache</TabsTrigger>
      </TabsList>

      <!-- ── Dashboard panel ──────────────────────────────────────── -->
      <TabsContent value="dashboard" class="space-y-6">
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
          <input
            v-model="dashProvider"
            class="bg-background focus:ring-ring h-9 w-40 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="provider"
          />
          <button
            @click="refreshDashboard()"
            :disabled="store.trendLoading || store.distLoading"
            class="hover:bg-accent ml-auto inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
          >
            <RefreshCw
              :class="store.trendLoading || store.distLoading ? 'animate-spin' : ''"
              class="h-3.5 w-3.5"
            />
            Refresh
          </button>
        </div>

        <div
          v-if="store.trendError || store.distError"
          class="bg-destructive/10 text-destructive border-destructive/30 flex items-start gap-3 rounded-lg border px-4 py-3"
          role="alert"
        >
          <AlertCircle class="mt-0.5 h-4 w-4 flex-shrink-0" />
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium">Failed to load dashboard data</p>
            <p class="mt-1 text-xs break-words opacity-80">
              {{ store.trendError || store.distError }}
            </p>
          </div>
        </div>

        <div class="grid grid-cols-1 gap-6 lg:grid-cols-2">
          <!-- Distribution pie chart -->
          <div class="bg-card rounded-lg border p-4">
            <h3 class="mb-3 text-sm font-medium">Call Distribution</h3>
            <div
              v-if="store.distLoading"
              class="text-muted-foreground flex items-center justify-center py-16"
            >
              <Loader2 class="h-5 w-5 animate-spin" />
            </div>
            <div
              v-else-if="store.distribution.length === 0"
              class="text-muted-foreground flex items-center justify-center py-16 text-sm"
            >
              No data available.
            </div>
            <div v-else class="mx-auto max-w-xs">
              <Pie
                :data="pieChart"
                :options="{ plugins: { legend: { position: 'bottom' as const } } }"
              />
            </div>
          </div>

          <!-- Token trend line chart -->
          <div class="bg-card rounded-lg border p-4">
            <h3 class="mb-3 text-sm font-medium">Token Trend</h3>
            <div
              v-if="store.trendLoading"
              class="text-muted-foreground flex items-center justify-center py-16"
            >
              <Loader2 class="h-5 w-5 animate-spin" />
            </div>
            <div
              v-else-if="store.trend.length === 0"
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
          </div>

          <!-- Call count trend line chart -->
          <div class="bg-card rounded-lg border p-4 lg:col-span-2">
            <h3 class="mb-3 text-sm font-medium">Call Count Trend</h3>
            <div
              v-if="store.trendLoading"
              class="text-muted-foreground flex items-center justify-center py-16"
            >
              <Loader2 class="h-5 w-5 animate-spin" />
            </div>
            <div
              v-else-if="store.trend.length === 0"
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
          </div>
        </div>
      </TabsContent>

      <!-- ── Stats panel (existing aggregate view) ──────────────────── -->
      <TabsContent value="stats" class="space-y-6">
        <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
          <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
            <TrendingUp class="h-5 w-5 text-blue-500" />
            <div>
              <p class="text-muted-foreground text-xs">Total Calls</p>
              <p class="text-lg font-bold tabular-nums">{{ statsTotals.calls.toLocaleString() }}</p>
            </div>
          </div>
          <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
            <Zap class="h-5 w-5 text-amber-500" />
            <div>
              <p class="text-muted-foreground text-xs">Total Tokens</p>
              <p class="text-lg font-bold tabular-nums">
                {{ statsTotals.tokens.toLocaleString() }}
              </p>
            </div>
          </div>
          <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
            <Clock class="h-5 w-5 text-green-500" />
            <div>
              <p class="text-muted-foreground text-xs">Avg Latency</p>
              <p class="text-lg font-bold tabular-nums">{{ formatDuration(statsTotals.avgMs) }}</p>
            </div>
          </div>
          <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
            <div
              class="flex h-5 w-5 items-center justify-center rounded-full border-2 border-red-500 text-[10px] font-bold text-red-500"
            >
              !
            </div>
            <div>
              <p class="text-muted-foreground text-xs">Errors</p>
              <p
                class="text-lg font-bold tabular-nums"
                :class="statsTotals.errors > 0 ? 'text-red-500' : ''"
              >
                {{ statsTotals.errors }}
              </p>
            </div>
          </div>
        </div>

        <div class="flex items-center gap-3">
          <input
            v-model="statsFilterProvider"
            @keyup.enter="refreshStats()"
            class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="Filter provider..."
          />
          <input
            v-model="statsFilterModel"
            @keyup.enter="refreshStats()"
            class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="Filter model..."
          />
          <button
            @click="refreshStats()"
            class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
          >
            <Search class="h-3.5 w-3.5" />
            Query
          </button>
          <button
            @click="refreshStats()"
            :disabled="store.statsLoading"
            class="hover:bg-accent ml-auto inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
          >
            <RefreshCw :class="store.statsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>

        <div
          v-if="store.statsError"
          class="bg-destructive/10 text-destructive border-destructive/30 flex items-start gap-3 rounded-lg border px-4 py-3"
          role="alert"
        >
          <AlertCircle class="mt-0.5 h-4 w-4 flex-shrink-0" />
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium">Failed to load statistics</p>
            <p class="mt-1 text-xs break-words opacity-80">{{ store.statsError }}</p>
          </div>
          <button
            @click="refreshStats()"
            class="bg-destructive text-destructive-foreground hover:bg-destructive/90 inline-flex h-8 items-center rounded-md px-3 text-xs font-medium transition-colors"
          >
            Retry
          </button>
        </div>

        <div class="bg-card rounded-lg border">
          <div v-if="store.statsLoading" class="text-muted-foreground py-12 text-center">
            <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
          </div>
          <div
            v-else-if="store.stats.length === 0"
            class="text-muted-foreground py-12 text-center text-sm"
          >
            No statistics available.
          </div>
          <table v-else class="w-full text-sm">
            <thead>
              <tr class="bg-muted/50 border-b">
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Provider</th>
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Model</th>
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Real Model</th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Calls</th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Tokens</th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">
                  Avg Latency
                </th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Errors</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(row, i) in store.stats"
                :key="i"
                class="hover:bg-muted/30 border-b last:border-0"
              >
                <td class="px-4 py-2.5 font-medium">{{ row.provider }}</td>
                <td class="px-4 py-2.5 font-mono text-xs">{{ row.model }}</td>
                <td class="px-4 py-2.5 font-mono text-xs">{{ row.real_model }}</td>
                <td class="px-4 py-2.5 text-right tabular-nums">
                  {{ row.call_count.toLocaleString() }}
                </td>
                <td class="px-4 py-2.5 text-right tabular-nums">
                  {{ row.total_tokens.toLocaleString() }}
                </td>
                <td class="px-4 py-2.5 text-right font-mono text-xs tabular-nums">
                  {{ formatDuration(row.avg_duration_ms) }}
                </td>
                <td
                  class="px-4 py-2.5 text-right tabular-nums"
                  :class="row.error_count > 0 ? 'font-medium text-red-500' : ''"
                >
                  {{ row.error_count }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </TabsContent>

      <!-- ── Cache panel ──────────────────────────────────────────── -->
      <TabsContent value="cache" class="space-y-4">
        <div class="flex items-center gap-3">
          <input
            v-model="cacheFilterProvider"
            @keyup.enter="refreshCache()"
            class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="Filter provider..."
          />
          <input
            v-model="cacheFilterModel"
            @keyup.enter="refreshCache()"
            class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="Filter model..."
          />
          <button
            @click="refreshCache()"
            class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
          >
            <Search class="h-3.5 w-3.5" />
            Query
          </button>
          <button
            @click="refreshCache()"
            :disabled="store.cacheLoading"
            class="hover:bg-accent ml-auto inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
          >
            <RefreshCw :class="store.cacheLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>

        <div
          v-if="store.cacheError"
          class="bg-destructive/10 text-destructive border-destructive/30 flex items-start gap-3 rounded-lg border px-4 py-3"
          role="alert"
        >
          <AlertCircle class="mt-0.5 h-4 w-4 flex-shrink-0" />
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium">Failed to load cache statistics</p>
            <p class="mt-1 text-xs break-words opacity-80">{{ store.cacheError }}</p>
          </div>
          <button
            @click="refreshCache()"
            class="bg-destructive text-destructive-foreground hover:bg-destructive/90 inline-flex h-8 items-center rounded-md px-3 text-xs font-medium transition-colors"
          >
            Retry
          </button>
        </div>

        <div class="bg-card rounded-lg border">
          <div v-if="store.cacheLoading" class="text-muted-foreground py-12 text-center">
            <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
          </div>
          <div
            v-else-if="store.cache.length === 0"
            class="text-muted-foreground py-12 text-center text-sm"
          >
            No cache statistics available.
          </div>
          <table v-else class="w-full text-sm">
            <thead>
              <tr class="bg-muted/50 border-b">
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Provider</th>
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Model</th>
                <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Real Model</th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Calls</th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">
                  Prompt Tokens
                </th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">
                  Cached Tokens
                </th>
                <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Hit Rate</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(row, i) in store.cache"
                :key="i"
                class="hover:bg-muted/30 border-b last:border-0"
              >
                <td class="px-4 py-2.5 font-medium">{{ row.provider }}</td>
                <td class="px-4 py-2.5 font-mono text-xs">{{ row.model }}</td>
                <td class="px-4 py-2.5 font-mono text-xs">{{ row.real_model }}</td>
                <td class="px-4 py-2.5 text-right tabular-nums">
                  {{ row.total_calls.toLocaleString() }}
                </td>
                <td class="px-4 py-2.5 text-right tabular-nums">
                  {{ row.total_prompt_tokens.toLocaleString() }}
                </td>
                <td class="px-4 py-2.5 text-right tabular-nums">
                  {{ row.total_cached_tokens.toLocaleString() }}
                </td>
                <td class="px-4 py-2.5 text-right font-mono text-xs tabular-nums">
                  <span
                    v-if="row.total_prompt_tokens > 0"
                    :class="
                      row.total_cached_tokens / row.total_prompt_tokens > 0
                        ? 'text-green-600'
                        : 'text-muted-foreground'
                    "
                  >
                    {{
                      ((row.total_cached_tokens / row.total_prompt_tokens) * 100).toFixed(1) + "%"
                    }}
                  </span>
                  <span v-else class="text-muted-foreground">—</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </TabsContent>

      <!-- ── Logs panel ────────────────────────────────────────────── -->
      <TabsContent value="logs" class="space-y-4">
        <div class="flex flex-wrap items-center gap-3">
          <input
            v-model="logsFilterProvider"
            @keyup.enter="refreshLogs()"
            class="bg-background focus:ring-ring h-9 w-40 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="provider"
          />
          <input
            v-model="logsFilterModel"
            @keyup.enter="refreshLogs()"
            class="bg-background focus:ring-ring h-9 w-40 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="model"
          />
          <input
            v-model="logsFilterRealModel"
            @keyup.enter="refreshLogs()"
            class="bg-background focus:ring-ring h-9 w-40 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="real_model"
          />
          <input
            v-model="logsFilterStatusCode"
            @keyup.enter="refreshLogs()"
            type="number"
            min="0"
            class="bg-background focus:ring-ring h-9 w-28 rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="status"
          />
          <button
            @click="refreshLogs()"
            class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
          >
            <Search class="h-3.5 w-3.5" />
            Query
          </button>
          <button
            @click="refreshLogs()"
            :disabled="store.logsLoading"
            class="hover:bg-accent ml-auto inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
          >
            <RefreshCw :class="store.logsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>

        <div
          v-if="store.logsError"
          class="bg-destructive/10 text-destructive border-destructive/30 flex items-start gap-3 rounded-lg border px-4 py-3"
          role="alert"
        >
          <AlertCircle class="mt-0.5 h-4 w-4 flex-shrink-0" />
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium">Failed to load call logs</p>
            <p class="mt-1 text-xs break-words opacity-80">{{ store.logsError }}</p>
          </div>
          <button
            @click="store.fetchLogs()"
            class="bg-destructive text-destructive-foreground hover:bg-destructive/90 inline-flex h-8 items-center rounded-md px-3 text-xs font-medium transition-colors"
          >
            Retry
          </button>
        </div>

        <div class="bg-card rounded-lg border">
          <div v-if="store.logsLoading" class="text-muted-foreground py-12 text-center">
            <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
          </div>
          <div
            v-else-if="store.logs.length === 0"
            class="text-muted-foreground py-12 text-center text-sm"
          >
            No call logs match the current filter.
          </div>
          <table v-else class="w-full text-sm">
            <thead>
              <tr class="bg-muted/50 border-b">
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Time</th>
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Provider</th>
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Model</th>
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Real Model</th>
                <th class="text-muted-foreground px-3 py-2 text-center font-medium">Stream</th>
                <th class="text-muted-foreground px-3 py-2 text-right font-medium">Status</th>
                <th class="text-muted-foreground px-3 py-2 text-right font-medium">Prompt</th>
                <th class="text-muted-foreground px-3 py-2 text-right font-medium">Completion</th>
                <th class="text-muted-foreground px-3 py-2 text-right font-medium">Total</th>
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Reasoning</th>
                <th class="text-muted-foreground px-3 py-2 text-right font-medium">Duration</th>
                <th class="text-muted-foreground px-3 py-2 text-left font-medium">Error</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="row in store.logs"
                :key="row.request_id"
                class="hover:bg-muted/30 border-b last:border-0"
              >
                <td class="text-muted-foreground px-3 py-2 font-mono text-xs">
                  {{ formatTs(row.timestamp) }}
                </td>
                <td class="px-3 py-2 font-medium">{{ row.provider }}</td>
                <td class="px-3 py-2 font-mono text-xs">{{ row.model }}</td>
                <td class="px-3 py-2 font-mono text-xs">{{ row.real_model }}</td>
                <td class="px-3 py-2 text-center">
                  <span
                    v-if="row.stream"
                    class="rounded bg-blue-500/15 px-1.5 py-0.5 text-[10px] font-semibold text-blue-600 dark:text-blue-400"
                    >SSE</span
                  >
                  <span v-else class="text-muted-foreground text-xs">—</span>
                </td>
                <td
                  class="px-3 py-2 text-right font-mono text-xs tabular-nums"
                  :class="
                    row.status_code >= 500
                      ? 'text-red-500'
                      : row.status_code >= 400
                        ? 'text-amber-500'
                        : 'text-green-600'
                  "
                >
                  {{ row.status_code }}
                </td>
                <td class="px-3 py-2 text-right tabular-nums">
                  {{ row.usage?.prompt_tokens?.toLocaleString() ?? "—" }}
                </td>
                <td class="px-3 py-2 text-right tabular-nums">
                  {{ row.usage?.completion_tokens?.toLocaleString() ?? "—" }}
                </td>
                <td class="px-3 py-2 text-right tabular-nums">
                  {{ row.usage?.total_tokens?.toLocaleString() ?? "—" }}
                </td>
                <td class="px-3 py-2 text-xs">
                  <span
                    v-if="row.reasoning_effort"
                    class="rounded bg-violet-500/15 px-1.5 py-0.5 font-medium text-violet-600 dark:text-violet-400"
                    >{{ row.reasoning_effort }}</span
                  >
                  <span v-else class="text-muted-foreground">—</span>
                </td>
                <td class="px-3 py-2 text-right font-mono text-xs tabular-nums">
                  <div :title="`${row.duration_ms} ms`">{{ formatDuration(row.duration_ms) }}</div>
                  <div
                    v-if="row.upstream_duration_ms != null"
                    class="text-muted-foreground text-[10px]"
                  >
                    upstream
                    <span :title="`${row.upstream_duration_ms} ms`">{{
                      formatDuration(row.upstream_duration_ms)
                    }}</span>
                  </div>
                  <div v-if="row.ttft_ms != null" class="text-muted-foreground text-[10px]">
                    ttft
                    <span :title="`${row.ttft_ms} ms`">{{ formatDuration(row.ttft_ms) }}</span>
                  </div>
                  <div v-if="fmtTps(row)" class="text-muted-foreground text-[10px]">
                    {{ fmtTps(row) }}
                  </div>
                </td>
                <td
                  class="text-muted-foreground max-w-[16rem] truncate px-3 py-2 font-mono text-xs"
                  :title="row.error"
                >
                  {{ row.error || "—" }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <!-- Pagination -->
        <div class="flex items-center justify-between text-sm">
          <div class="text-muted-foreground">
            Page <span class="text-foreground font-medium tabular-nums">{{ logsCurrentPage }}</span>
            <span v-if="store.logs.length > 0">
              · showing {{ store.logs.length }} row{{ store.logs.length === 1 ? "" : "s" }}
            </span>
          </div>
          <div class="flex items-center gap-2">
            <button
              @click="store.prevLogsPage()"
              :disabled="store.logsOffset === 0 || store.logsLoading"
              class="hover:bg-accent inline-flex h-8 items-center gap-1 rounded-md border px-2.5 text-sm transition-colors disabled:opacity-50"
            >
              <ChevronLeft class="h-3.5 w-3.5" /> Prev
            </button>
            <button
              @click="store.nextLogsPage()"
              :disabled="!store.hasMoreLogs || store.logsLoading"
              class="hover:bg-accent inline-flex h-8 items-center gap-1 rounded-md border px-2.5 text-sm transition-colors disabled:opacity-50"
            >
              Next <ChevronRight class="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      </TabsContent>
    </Tabs>
  </div>
</template>

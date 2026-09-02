<script setup lang="ts">
import {
  Activity,
  AlertCircle,
  ArrowDown,
  ArrowUp,
  ArrowUpDown,
  ArrowUpRight,
  Clock,
  Database,
  Gauge,
  Info,
  Loader2,
  Search,
  Timer,
  TrendingUp,
  Zap,
} from "@lucide/vue"
import { computed, onMounted, ref, watch } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import SearchSelect from "@/components/SearchSelect.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { formatCompactNumber, successRateColor } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type {
  AnalyticsGroupKey,
  AnalyticsGroupSummary,
  AnalyticsScope,
  AnalyticsV2Query,
  LatencyMetric,
  MetricSummary,
} from "@/types/api"
import DateRangePicker from "./DateRangePicker.vue"
import type { DateRangeMs } from "./DateRangePicker.vue"
import {
  formatCount,
  formatLatency,
  formatRate,
  formatRatePerSec,
  groupKeyLabel,
  populationLabel,
  populationOf,
  throughputUnit,
} from "./v2"
import V2TrendChart from "./V2TrendChart.vue"
const store = useAnalyticsStore()
const providerStore = useProviderStore()
const routeStore = useRouteStore()

// ── Filter option sources (mirror LogsPage) ──────────────────────
const routeOptions = computed(() => routeStore.routes.map((r) => ({ value: r.model_name })))
const providerOptions = computed(() =>
  providerStore.providers.map((p) => ({ value: p.name, label: p.name })),
)
const modelOptions = computed(() => providerStore.allProviderModels.map((m) => ({ value: m })))

onMounted(() => {
  // Load option sources for the SearchSelect filters if not already loaded.
  if (providerStore.providers.length === 0) providerStore.fetchProviders()
  if (providerStore.allProviderModels.length === 0) providerStore.fetchAllProviderModels()
  if (routeStore.routes.length === 0) routeStore.fetchRoutes()
})

// ── Query state ────────────────────────────────────────────────────
type TimeRange = "1h" | "24h" | "7d" | "custom"
const RANGE_MS: Record<Exclude<TimeRange, "custom">, number> = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 604_800_000,
}
const SCOPE_OPTIONS: AnalyticsScope[] = ["route", "target"]

const range = ref<TimeRange>("24h")
/** Custom range (UTC ms bounds) picked via the calendar range picker. */
const customRange = ref<DateRangeMs | null>(null)
const scope = ref<AnalyticsScope>("route")
const routeFilter = ref("")
const providerFilter = ref("")
const modelFilter = ref("")

const loading = computed(() => store.v2SummaryLoading || store.v2TrendLoading)
const summaryLoading = computed(() => store.v2SummaryLoading)
const trendLoading = computed(() => store.v2TrendLoading)
const summaryError = computed(() => store.v2SummaryError)
const trendError = computed(() => store.v2TrendError)

/** Non-zero while a refresh is in flight. The store keeps the previous
 *  query's data until the new response lands — rendering it under the new
 *  scope / range label would mislead, so the panel treats it as absent
 *  (a loading guard) instead of clearing shared store state. */
const inFlight = ref(0)
const dataStale = computed(() => inFlight.value > 0)

/** The last query actually issued by `refresh()`. Responses are only shown
 *  when they match it (scope + interval), so an out-of-order response from
 *  an older scope/range can never be paired with the current labels. */
const lastQuery = ref<AnalyticsV2Query | null>(null)

/**
 * Identity check of a response against the query that issued it. All three
 * fields are compared against `lastQuery` (`q.scope` / `q.from` / `q.to`) —
 * never against the current selection, whose refs are bound by the user
 * while `lastQuery` is bound by `refresh()`, so mixing the two would let a
 * late response slip in under changed labels.
 */
function matchesLastQuery(res: { scope: AnalyticsScope; from_ms: number; to_ms: number } | null) {
  const q = lastQuery.value
  if (!q || !res) return false
  return res.scope === q.scope && res.from_ms === q.from && res.to_ms === q.to
}

const summary = computed(() => {
  if (dataStale.value || summaryError.value) return null
  return matchesLastQuery(store.v2Summary) ? store.v2Summary : null
})
const overall = computed(() => summary.value?.overall ?? null)
const groups = computed(() => summary.value?.groups ?? [])
const unassigned = computed(() => summary.value?.unassigned_failures ?? [])
const trend = computed(() => {
  if (dataStale.value || trendError.value) return null
  return matchesLastQuery(store.v2Trend) ? store.v2Trend : null
})

/** True when a custom range is selected but from/to are missing or
 *  invalid; the Query button is disabled and `refresh()` no-ops then. */
const customRangeInvalid = computed(() => {
  if (range.value !== "custom") return false
  const r = customRange.value
  return r === null || r.fromMs >= r.toMs
})

function buildQuery(): AnalyticsV2Query | null {
  const now = Date.now()
  const isTarget = scope.value === "target"
  let from: number
  let to: number
  if (range.value === "custom") {
    const r = customRange.value
    if (r === null || r.fromMs >= r.toMs) return null
    from = r.fromMs
    to = r.toMs
  } else {
    from = now - RANGE_MS[range.value]
    to = now
  }
  return {
    from,
    to,
    scope: scope.value,
    route: routeFilter.value.trim() || undefined,
    // provider / real_model are target-scope-only filters; a route-scope
    // query with leftover filter text would be rejected by the backend.
    provider: isTarget ? providerFilter.value.trim() || undefined : undefined,
    real_model: isTarget ? modelFilter.value.trim() || undefined : undefined,
  }
}

async function refresh() {
  const query = buildQuery()
  if (!query) return
  lastQuery.value = query
  inFlight.value++
  try {
    await Promise.all([store.fetchV2Summary(query), store.fetchV2Trend(query)])
  } finally {
    inFlight.value--
  }
}

// Auto-refresh on scope changes and on preset range switches; the custom
// range only takes effect via Query after the user picks from/to (picking
// in the calendar must not fire requests).
watch(scope, () => refresh(), { immediate: true })
watch(range, (r) => {
  if (r !== "custom") refresh()
})

// ── Summary cards ──────────────────────────────────────────────────
const latencyCards = computed<
  Array<{ key: string; title: string; icon: any; metric: LatencyMetric }>
>(() => {
  const m = overall.value
  if (!m) return []
  return [
    { key: "e2e", title: "E2E", icon: Clock, metric: m.e2e },
    { key: "ttft", title: "TTFT", icon: Timer, metric: m.ttft },
    { key: "upstream", title: "Upstream", icon: ArrowUpRight, metric: m.upstream },
  ]
})

// ── Groups table ───────────────────────────────────────────────────
interface GroupColumn {
  key: string
  label: string
  align: "left" | "right"
}

interface GroupCell {
  value: string
  /** 0..1 success-rate fraction when the cell is a rate (colored). */
  rate?: number | null
}

interface GroupRow {
  key: string
  cells: GroupCell[]
  /** True for the synthetic `others` aggregation row (muted/italic). */
  others?: boolean
}

const groupColumns = computed<GroupColumn[]>(() => {
  const right = "right" as const
  const left = "left" as const
  if (scope.value === "target") {
    return [
      { key: "provider", label: "Provider", align: left },
      { key: "real_model", label: "Actual Model", align: left },
      { key: "attempts", label: "Attempts", align: right },
      { key: "success", label: "Success", align: right },
      { key: "failure", label: "Failure", align: right },
      { key: "success_rate", label: "Success Rate", align: right },
      { key: "avg_e2e", label: "Avg E2E", align: right },
      { key: "p95_e2e", label: "P95 E2E", align: right },
      { key: "tokens_in", label: "Tokens In", align: right },
      { key: "tokens_out", label: "Tokens Out", align: right },
      { key: "cache_hit", label: "Cache Hit", align: right },
      { key: "cache_unknown", label: "Cache Unknown", align: right },
    ]
  }
  return [
    { key: "route_name", label: "Route Name", align: left },
    { key: "requests", label: "Requests", align: right },
    { key: "success", label: "Success", align: right },
    { key: "success_rate", label: "Success Rate", align: right },
    { key: "model_failure_rate", label: "Model Failure", align: right },
    { key: "avg_e2e", label: "Avg E2E", align: right },
    { key: "p95_e2e", label: "P95 E2E", align: right },
    { key: "tokens_in", label: "Tokens In", align: right },
    { key: "tokens_out", label: "Tokens Out", align: right },
    { key: "cache_hit", label: "Cache Hit", align: right },
    { key: "cache_unknown", label: "Cache Unknown", align: right },
  ]
})

function isRouteKey(key: AnalyticsGroupKey): key is { route_name: string } {
  return "route_name" in key
}

/** Stable row identity, kept separate from the labels shown in the table. */
function groupRowKey(key: AnalyticsGroupKey): string {
  if (isRouteKey(key)) return `route:${JSON.stringify(key.route_name)}`
  return `target:${JSON.stringify([key.provider, key.real_model])}`
}

const OTHERS_ROW_KEY = "__analytics_v2_others__"

/** 按当前排序从全量 groups 取最多 20 个具名行；其余聚合成 `others` 行。 */
const GROUPS_TOP_N = 20

// ── Sorting state ──────────────────────────────────────────────────
const sortKey = ref<string | null>(null)
const sortOrder = ref<"asc" | "desc">("desc")

function toggleSort(key: string) {
  if (sortKey.value === key) {
    if (sortOrder.value === "desc") {
      sortOrder.value = "asc"
    } else {
      sortKey.value = null
      sortOrder.value = "desc"
    }
  } else {
    sortKey.value = key
    const col = groupColumns.value.find((c) => c.key === key)
    sortOrder.value = col?.align === "left" ? "asc" : "desc"
  }
}

watch(scope, () => {
  sortKey.value = null
  sortOrder.value = "desc"
})

function getSortValue(g: AnalyticsGroupSummary, key: string): string | number | null {
  const m = g.metrics
  switch (key) {
    case "route_name":
      return groupKeyLabel(g.key)
    case "provider":
      return isRouteKey(g.key) ? "" : g.key.provider
    case "real_model":
      return isRouteKey(g.key) ? "" : g.key.real_model
    case "requests":
      return m.requests
    case "attempts":
      return m.attempts
    case "success":
      return m.success
    case "failure":
      return m.failure
    case "success_rate":
      return m.success_rate
    case "model_failure_rate":
      return m.model_failure_rate
    case "avg_e2e":
      return m.e2e.avg_ms
    case "p95_e2e":
      return m.e2e.p95_ms
    case "tokens_in":
      return m.prompt_tokens
    case "tokens_out":
      return m.completion_tokens
    case "cache_hit":
      return m.request_cache_hit_rate
    case "cache_unknown":
      return m.cache_unknown
    default:
      return 0
  }
}

const sortedGroups = computed<AnalyticsGroupSummary[]>(() => {
  const list = [...groups.value]
  if (!sortKey.value) return list

  const key = sortKey.value
  const order = sortOrder.value === "asc" ? 1 : -1

  return list.sort((a, b) => {
    const valA = getSortValue(a, key)
    const valB = getSortValue(b, key)

    // Missing metrics always sort after valid values, regardless of direction.
    if (valA === null && valB === null) return 0
    if (valA === null) return 1
    if (valB === null) return -1

    if (typeof valA === "string" && typeof valB === "string") {
      return valA.localeCompare(valB) * order
    }
    return (Number(valA) - Number(valB)) * order
  })
})

function buildGroupRow(g: AnalyticsGroupSummary): GroupRow {
  const m = g.metrics
  const cells: GroupCell[] = []
  for (const col of groupColumns.value) {
    switch (col.key) {
      case "route_name":
        cells.push({ value: groupKeyLabel(g.key) })
        break
      case "provider":
        cells.push({ value: isRouteKey(g.key) ? "—" : g.key.provider })
        break
      case "real_model":
        cells.push({ value: isRouteKey(g.key) ? "—" : g.key.real_model })
        break
      case "requests":
        cells.push({ value: m.requests.toLocaleString() })
        break
      case "attempts":
        cells.push({ value: m.attempts.toLocaleString() })
        break
      case "success":
        cells.push({ value: m.success.toLocaleString() })
        break
      case "failure":
        cells.push({ value: m.failure.toLocaleString() })
        break
      case "success_rate":
        cells.push({ value: formatRate(m.success_rate), rate: m.success_rate })
        break
      case "model_failure_rate":
        cells.push({ value: formatRate(m.model_failure_rate) })
        break
      case "avg_e2e":
        cells.push({ value: formatLatency(m.e2e.avg_ms) })
        break
      case "p95_e2e":
        cells.push({ value: formatLatency(m.e2e.p95_ms) })
        break
      case "tokens_in":
        cells.push({ value: formatCompactNumber(m.prompt_tokens) })
        break
      case "tokens_out":
        cells.push({ value: formatCompactNumber(m.completion_tokens) })
        break
      case "cache_hit":
        cells.push({ value: formatRate(m.request_cache_hit_rate) })
        break
      case "cache_unknown":
        cells.push({ value: m.cache_unknown.toLocaleString() })
        break
    }
  }
  return { key: groupRowKey(g.key), cells }
}

/** 把超出 Top-N 的组聚合成一个 `others` 行：可加性字段求和，率按原口径重算，
 *  分位数延迟不聚合（置 —）。仅在前端聚合，不改后端/store/types。 */
function buildOthersRow(overflow: AnalyticsGroupSummary[]): GroupRow {
  const sum = (pick: (m: MetricSummary) => number) =>
    overflow.reduce((acc, g) => acc + pick(g.metrics), 0)
  const requests = sum((m) => m.requests)
  const attempts = sum((m) => m.attempts)
  const success = sum((m) => m.success)
  const failure = sum((m) => m.failure)
  const model_failure = sum((m) => m.model_failure)
  const prompt_tokens = sum((m) => m.prompt_tokens)
  const completion_tokens = sum((m) => m.completion_tokens)
  const cache_known = sum((m) => m.cache_known)
  const cache_hit = sum((m) => m.cache_hit)
  const cache_unknown = sum((m) => m.cache_unknown)
  const isTarget = scope.value === "target"
  const cells: GroupCell[] = []
  for (const col of groupColumns.value) {
    switch (col.key) {
      case "route_name":
      case "provider":
      case "real_model":
        cells.push({ value: "others" })
        break
      case "requests":
        cells.push({ value: requests.toLocaleString() })
        break
      case "attempts":
        cells.push({ value: attempts.toLocaleString() })
        break
      case "success":
        cells.push({ value: success.toLocaleString() })
        break
      case "failure":
        cells.push({ value: failure.toLocaleString() })
        break
      case "success_rate": {
        const denom = isTarget ? attempts : requests
        const rate = denom > 0 ? success / denom : null
        cells.push({ value: rate === null ? "—" : formatRate(rate), rate })
        break
      }
      case "model_failure_rate": {
        const denom = success + model_failure
        const rate = denom > 0 ? model_failure / denom : null
        cells.push({ value: rate === null ? "—" : formatRate(rate) })
        break
      }
      case "avg_e2e":
      case "p95_e2e":
        cells.push({ value: "—" })
        break
      case "tokens_in":
        cells.push({ value: formatCompactNumber(prompt_tokens) })
        break
      case "tokens_out":
        cells.push({ value: formatCompactNumber(completion_tokens) })
        break
      case "cache_hit": {
        const rate = cache_known > 0 ? cache_hit / cache_known : null
        cells.push({ value: rate === null ? "—" : formatRate(rate) })
        break
      }
      case "cache_unknown":
        cells.push({ value: cache_unknown.toLocaleString() })
        break
    }
  }
  return { key: OTHERS_ROW_KEY, cells, others: true }
}

const groupRows = computed<GroupRow[]>(() => {
  const list = sortedGroups.value
  const rows = list.slice(0, GROUPS_TOP_N).map(buildGroupRow)
  if (list.length > GROUPS_TOP_N) {
    rows.push(buildOthersRow(list.slice(GROUPS_TOP_N)))
  }
  return rows
})
</script>

<template>
  <div class="flex h-full flex-col">
    <!-- ── Toolbar ─────────────────────────────────────────────── -->
    <div class="shrink-0">
      <div class="-mx-1 flex flex-nowrap items-center gap-2 overflow-x-auto px-1 py-1">
        <div class="bg-muted inline-flex shrink-0 rounded-md p-0.5 whitespace-nowrap">
          <button
            v-for="s in SCOPE_OPTIONS"
            :key="s"
            @click="scope = s"
            class="rounded-sm px-3 py-1 text-sm transition-colors"
            :class="scope === s ? 'bg-background font-medium shadow-sm' : 'text-muted-foreground'"
          >
            {{ s === "route" ? "Route" : "Target" }}
          </button>
        </div>
        <Select
          :model-value="range"
          @update:model-value="(v) => (range = v as TimeRange)"
          class="shrink-0 whitespace-nowrap"
        >
          <SelectTrigger class="w-28">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="1h">1h</SelectItem>
            <SelectItem value="24h">24h</SelectItem>
            <SelectItem value="7d">7d</SelectItem>
            <SelectItem value="custom">Custom</SelectItem>
          </SelectContent>
        </Select>
        <DateRangePicker
          v-if="range === 'custom'"
          v-model="customRange"
          class="shrink-0"
          placeholder="Pick a range"
        />
        <div class="flex shrink-0 items-center gap-2">
          <SearchSelect
            v-model="routeFilter"
            :options="routeOptions"
            placeholder="route"
            search-placeholder="Search route..."
            class="w-40 shrink-0"
          />
          <SearchSelect
            v-if="scope === 'target'"
            v-model="providerFilter"
            :options="providerOptions"
            placeholder="provider"
            search-placeholder="Search provider..."
            class="w-40 shrink-0"
          />
          <SearchSelect
            v-if="scope === 'target'"
            v-model="modelFilter"
            :options="modelOptions"
            placeholder="actual model"
            search-placeholder="Search actual model..."
            class="w-40 shrink-0"
          />
        </div>
        <Button
          size="sm"
          class="shrink-0 whitespace-nowrap"
          :disabled="customRangeInvalid"
          @click="refresh()"
        >
          <Search class="h-3.5 w-3.5" />
          Query
        </Button>
        <RefreshButton
          :loading="loading"
          size="default"
          class="ml-auto shrink-0 whitespace-nowrap"
          @click="refresh()"
        />
      </div>
    </div>

    <!-- ── Content ─────────────────────────────────────────────── -->
    <div class="min-h-0 flex-1 space-y-4 overflow-y-auto pt-4">
      <div
        v-if="loading && !overall && !trend"
        class="text-muted-foreground flex items-center justify-center gap-2 py-16"
      >
        <Loader2 class="h-5 w-5 animate-spin" /> Loading...
      </div>
      <div
        v-else-if="!overall && !trend && !summaryError && !trendError"
        class="text-muted-foreground flex items-center justify-center py-16 text-sm"
      >
        No analytics data in this range. Adjust filters and click Query.
      </div>

      <template v-else>
        <!-- ── Summary section ────────────────────────────────── -->
        <Alert v-if="summaryError" variant="destructive">
          <AlertCircle class="h-4 w-4" />
          <AlertTitle>Failed to load summary</AlertTitle>
          <AlertDescription class="break-words">{{ summaryError }}</AlertDescription>
          <Button variant="destructive" size="sm" class="mt-2" @click="refresh()">Retry</Button>
        </Alert>
        <div
          v-else-if="summaryLoading && !overall"
          class="text-muted-foreground flex items-center justify-center gap-2 py-8"
        >
          <Loader2 class="h-4 w-4 animate-spin" /> Loading summary...
        </div>
        <div
          v-else-if="!overall"
          class="text-muted-foreground flex items-center justify-center py-8 text-sm"
        >
          No summary data in this range.
        </div>

        <!-- ── Summary cards ───────────────────────────────────── -->
        <template v-else>
          <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
            <Card>
              <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                <TrendingUp class="h-5 w-5 text-blue-500" />
                <div>
                  <p class="text-muted-foreground text-xs">{{ populationLabel(scope) }}</p>
                  <p class="text-lg font-bold tabular-nums xl:text-xl">
                    {{ populationOf(overall, scope).toLocaleString() }}
                  </p>
                  <p class="text-muted-foreground text-xs">
                    Success {{ overall.success.toLocaleString() }} · Failure
                    {{ overall.failure.toLocaleString() }}
                  </p>
                </div>
              </CardContent>
            </Card>
            <Card>
              <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                <Activity class="h-5 w-5 text-green-500" />
                <div>
                  <p class="text-muted-foreground text-xs">Success Rate</p>
                  <p
                    v-if="overall.success_rate !== null"
                    class="text-lg font-bold tabular-nums xl:text-xl"
                    :style="{ color: successRateColor(overall.success_rate * 100) }"
                  >
                    {{ formatRate(overall.success_rate) }}
                  </p>
                  <p v-else class="text-muted-foreground text-lg">—</p>
                  <p class="text-muted-foreground text-xs">
                    Failure {{ formatRate(overall.overall_failure_rate) }}
                  </p>
                </div>
              </CardContent>
            </Card>
            <Card>
              <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                <Zap class="h-5 w-5 text-amber-500" />
                <div>
                  <p class="text-muted-foreground text-xs">Input Tokens</p>
                  <p class="text-lg font-bold tabular-nums xl:text-xl">
                    {{ formatCompactNumber(overall.prompt_tokens) }}
                  </p>
                  <p class="text-muted-foreground text-xs">
                    Out {{ formatCompactNumber(overall.completion_tokens) }} · Total
                    {{ formatCompactNumber(overall.total_tokens) }}
                  </p>
                </div>
              </CardContent>
            </Card>
            <Card>
              <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                <Gauge class="h-5 w-5 text-purple-500" />
                <div>
                  <p class="text-muted-foreground text-xs">Throughput</p>
                  <p class="text-lg font-bold tabular-nums xl:text-xl">
                    {{
                      overall.request_throughput_per_sec !== null
                        ? `${overall.request_throughput_per_sec.toFixed(2)} ${throughputUnit(scope)}`
                        : "—"
                    }}
                  </p>
                  <p class="text-muted-foreground text-xs">
                    In {{ formatRatePerSec(overall.input_token_throughput_per_sec) }} · Out
                    {{ formatRatePerSec(overall.output_token_throughput_per_sec) }} tokens/s
                  </p>
                </div>
              </CardContent>
            </Card>
          </div>

          <!-- ── Cache + latency cards ───────────────────────────── -->
          <div class="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
            <Card>
              <CardContent class="p-4">
                <div class="flex items-center justify-between">
                  <p class="text-muted-foreground flex items-center gap-1.5 text-xs">
                    <Database class="h-3.5 w-3.5" /> Cache Hit (requests)
                  </p>
                  <Badge
                    v-if="overall.cache_unknown > 0"
                    variant="outline"
                    class="border-amber-500/50 text-[10px] text-amber-600 dark:text-amber-400"
                  >
                    unknown {{ overall.cache_unknown }}
                  </Badge>
                </div>
                <p class="text-lg font-bold tabular-nums xl:text-xl">
                  {{ formatRate(overall.request_cache_hit_rate) }}
                </p>
                <p class="text-muted-foreground text-xs">
                  Token hit {{ formatRate(overall.token_cache_hit_rate) }}
                  <template v-if="overall.cache_known > 0">
                    · {{ overall.cache_known.toLocaleString() }} known
                  </template>
                  <template v-if="overall.cache_unknown > 0">
                    · {{ overall.cache_unknown.toLocaleString() }} unknown
                  </template>
                </p>
              </CardContent>
            </Card>

            <Card v-for="lc in latencyCards" :key="lc.key">
              <CardContent class="p-4">
                <div class="flex items-center justify-between">
                  <p class="text-muted-foreground flex items-center gap-1.5 text-xs">
                    <component :is="lc.icon" class="h-3.5 w-3.5" /> {{ lc.title }} Latency
                  </p>
                  <div class="flex items-center gap-1">
                    <Badge
                      v-if="lc.metric.approx_percentiles"
                      variant="outline"
                      class="border-amber-500/50 text-[10px] text-amber-600 dark:text-amber-400"
                      title="历史区间由 1h Rollup 直方图估算（≈1% 误差），非精确分位数"
                    >
                      ≈ approx
                    </Badge>
                    <Badge
                      v-if="lc.metric.low_sample"
                      variant="outline"
                      class="border-amber-500/50 text-[10px] text-amber-600 dark:text-amber-400"
                    >
                      low sample
                    </Badge>
                  </div>
                </div>
                <p class="text-lg font-bold tabular-nums xl:text-xl">
                  {{ formatLatency(lc.metric.avg_ms) }}
                </p>
                <p class="text-muted-foreground text-xs">
                  P50 {{ formatLatency(lc.metric.p50_ms) }} · P95
                  {{ formatLatency(lc.metric.p95_ms) }} · P99
                  {{ formatLatency(lc.metric.p99_ms) }}
                </p>
                <p class="text-muted-foreground text-[11px]">
                  coverage {{ formatRate(lc.metric.coverage) }} ·
                  {{ formatCount(lc.metric.samples) }}/{{ lc.metric.population.toLocaleString() }}
                  samples
                </p>
              </CardContent>
            </Card>
          </div>

          <!-- ── Unassigned failures ─────────────────────────────── -->
          <div
            v-if="scope === 'route' && unassigned.length > 0"
            class="flex flex-wrap items-center gap-2"
          >
            <span class="text-muted-foreground text-xs">Unassigned failures:</span>
            <Badge
              v-for="u in unassigned"
              :key="u.category"
              variant="outline"
              class="font-mono text-[11px]"
            >
              {{ u.category }} × {{ u.count }}
            </Badge>
          </div>

          <!-- ── Groups table ────────────────────────────────────── -->
          <div class="max-h-[32rem] overflow-auto rounded-lg border">
            <Table class="min-w-max">
              <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
                <TableRow>
                  <TableHead
                    v-for="col in groupColumns"
                    :key="col.key"
                    :class="[
                      col.align === 'right' ? 'text-right' : 'text-left',
                      'hover:text-foreground cursor-pointer transition-colors select-none',
                    ]"
                    @click="toggleSort(col.key)"
                  >
                    <div
                      class="inline-flex items-center gap-1.5"
                      :class="col.align === 'right' ? 'justify-end' : ''"
                    >
                      <template v-if="col.key === 'cache_unknown'">
                        <span>{{ col.label }}</span>
                        <Tooltip>
                          <TooltipTrigger as-child>
                            <Info
                              class="text-muted-foreground h-3.5 w-3.5 cursor-help"
                              @click.stop
                            />
                          </TooltipTrigger>
                          <TooltipContent side="top" class="max-w-xs font-normal">
                            成功样本中未上报 cached_tokens 的数量（unknown ≠
                            miss，不计入命中率分母）
                          </TooltipContent>
                        </Tooltip>
                      </template>
                      <template v-else>
                        <span>{{ col.label }}</span>
                      </template>

                      <span class="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center">
                        <ArrowUp
                          v-if="sortKey === col.key && sortOrder === 'asc'"
                          class="text-foreground h-3.5 w-3.5"
                        />
                        <ArrowDown
                          v-else-if="sortKey === col.key && sortOrder === 'desc'"
                          class="text-foreground h-3.5 w-3.5"
                        />
                        <ArrowUpDown
                          v-else
                          class="text-muted-foreground/30 hover:text-foreground h-3 w-3"
                        />
                      </span>
                    </div>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                <TableRow
                  v-for="row in groupRows"
                  :key="row.key"
                  :class="row.others ? 'text-muted-foreground italic' : ''"
                >
                  <TableCell
                    v-for="(cell, i) in row.cells"
                    :key="i"
                    :class="
                      groupColumns[i].align === 'right' ? 'text-right tabular-nums' : 'font-medium'
                    "
                  >
                    <span
                      v-if="cell.rate != null"
                      class="font-mono text-xs"
                      :style="{ color: successRateColor(cell.rate * 100) }"
                    >
                      {{ cell.value }}
                    </span>
                    <span v-else-if="cell.value === '—'" class="text-muted-foreground">
                      {{ cell.value }}
                    </span>
                    <span
                      v-else
                      :class="groupColumns[i].align === 'right' ? 'font-mono text-xs' : ''"
                    >
                      {{ cell.value }}
                    </span>
                  </TableCell>
                </TableRow>
                <TableRow v-if="groupRows.length === 0">
                  <TableCell
                    :colspan="groupColumns.length"
                    class="text-muted-foreground py-6 text-center"
                  >
                    No groups in this range.
                  </TableCell>
                </TableRow>
              </TableBody>
            </Table>
          </div>
        </template>

        <!-- ── Trend section ───────────────────────────────────── -->
        <Card>
          <CardContent class="space-y-4 p-4">
            <Alert v-if="trendError" variant="destructive">
              <AlertCircle class="h-4 w-4" />
              <AlertTitle>Failed to load trend</AlertTitle>
              <AlertDescription class="break-words">{{ trendError }}</AlertDescription>
              <Button variant="destructive" size="sm" class="mt-2" @click="refresh()">Retry</Button>
            </Alert>
            <div
              v-else-if="trendLoading && !trend"
              class="text-muted-foreground flex items-center justify-center gap-2 py-12"
            >
              <Loader2 class="h-4 w-4 animate-spin" /> Loading trend...
            </div>
            <div v-else-if="trend">
              <V2TrendChart :trend="trend" :scope="scope" />
            </div>
            <div
              v-else
              class="text-muted-foreground flex items-center justify-center py-12 text-sm"
            >
              No trend data available.
            </div>
          </CardContent>
        </Card>
      </template>
    </div>
  </div>
</template>

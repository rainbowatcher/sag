<script setup lang="ts">
import {
  Activity,
  AlertCircle,
  ArrowUpRight,
  Clock,
  Database,
  Gauge,
  Loader2,
  Search,
  Timer,
  TrendingUp,
  Zap,
} from "@lucide/vue"
import { computed, ref, watch } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { formatCompactNumber, successRateColor } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"
import type {
  AnalyticsGroupKey,
  AnalyticsGroupSummary,
  AnalyticsScope,
  AnalyticsV2Query,
  LatencyMetric,
} from "@/types/api"
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

// ── Query state ────────────────────────────────────────────────────
type TimeRange = "1h" | "24h" | "7d"
const RANGE_OPTIONS: TimeRange[] = ["1h", "24h", "7d"]
const RANGE_MS: Record<TimeRange, number> = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 604_800_000,
}
const SCOPE_OPTIONS: AnalyticsScope[] = ["route", "target"]

const range = ref<TimeRange>("24h")
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
const series = computed(() => trend.value?.series ?? [])
const others = computed(() => trend.value?.others ?? null)

function buildQuery(): AnalyticsV2Query {
  const now = Date.now()
  const isTarget = scope.value === "target"
  return {
    from: now - RANGE_MS[range.value],
    to: now,
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
  lastQuery.value = query
  inFlight.value++
  try {
    await Promise.all([store.fetchV2Summary(query), store.fetchV2Trend(query)])
  } finally {
    inFlight.value--
  }
}

// Auto-refresh on time-range / scope changes (filters apply via Query).
watch([range, scope], () => refresh(), { immediate: true })

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
}

const groupColumns = computed<GroupColumn[]>(() => {
  const right = "right" as const
  const left = "left" as const
  if (scope.value === "target") {
    return [
      { key: "provider", label: "Provider", align: left },
      { key: "real_model", label: "Actual Model", align: left },
      { key: "attempts", label: "尝试次数", align: right },
      { key: "success", label: "Success", align: right },
      { key: "failure", label: "Failure", align: right },
      { key: "success_rate", label: "Success Rate", align: right },
      { key: "avg_e2e", label: "Avg E2E", align: right },
      { key: "p95_e2e", label: "P95 E2E", align: right },
      { key: "tokens_in", label: "Tokens In", align: right },
      { key: "tokens_out", label: "Tokens Out", align: right },
      { key: "cache_hit", label: "Cache Hit", align: right },
      { key: "cache_unknown", label: "Unknown", align: right },
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
    { key: "cache_unknown", label: "Unknown", align: right },
  ]
})

function isRouteKey(key: AnalyticsGroupKey): key is { route_name: string } {
  return "route_name" in key
}

const groupRows = computed<GroupRow[]>(() =>
  groups.value.map((g: AnalyticsGroupSummary) => {
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
    return { key: groupKeyLabel(g.key), cells }
  }),
)

// ── Trend series table ─────────────────────────────────────────────
interface SeriesRow {
  label: string
  total: number
  success: number
  successRate: number | null
  tokensIn: number
  tokensOut: number
  others: boolean
}

function sumPoints(
  points: { success: number; prompt_tokens: number; completion_tokens: number }[],
) {
  return points.reduce(
    (acc, p) => ({
      success: acc.success + p.success,
      tokensIn: acc.tokensIn + p.prompt_tokens,
      tokensOut: acc.tokensOut + p.completion_tokens,
    }),
    { success: 0, tokensIn: 0, tokensOut: 0 },
  )
}

const seriesRows = computed<SeriesRow[]>(() => {
  const rows: SeriesRow[] = series.value.map((s) => {
    const totals = sumPoints(s.points)
    return {
      label: groupKeyLabel(s.key),
      total: s.total_requests,
      success: totals.success,
      successRate: s.total_requests > 0 ? totals.success / s.total_requests : null,
      tokensIn: totals.tokensIn,
      tokensOut: totals.tokensOut,
      others: false,
    }
  })
  const o = others.value
  if (o) {
    const totals = sumPoints(o.points)
    rows.push({
      label: "others",
      total: o.total_requests,
      success: totals.success,
      successRate: o.total_requests > 0 ? totals.success / o.total_requests : null,
      tokensIn: totals.tokensIn,
      tokensOut: totals.tokensOut,
      others: true,
    })
  }
  return rows
})
</script>

<template>
  <div class="flex h-full flex-col">
    <!-- ── Toolbar ─────────────────────────────────────────────── -->
    <div class="shrink-0">
      <div class="flex flex-wrap items-center gap-3">
        <div class="bg-muted inline-flex rounded-md p-0.5">
          <button
            v-for="r in RANGE_OPTIONS"
            :key="r"
            @click="range = r"
            class="rounded-sm px-3 py-1 text-sm transition-colors"
            :class="range === r ? 'bg-background font-medium shadow-sm' : 'text-muted-foreground'"
          >
            {{ r }}
          </button>
        </div>
        <div class="bg-muted inline-flex rounded-md p-0.5">
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
        <Input
          v-model="routeFilter"
          class="w-40 xl:w-52"
          placeholder="Filter route..."
          @keyup.enter="refresh()"
        />
        <Input
          v-if="scope === 'target'"
          v-model="providerFilter"
          class="w-40 xl:w-52"
          placeholder="Filter provider..."
          @keyup.enter="refresh()"
        />
        <Input
          v-if="scope === 'target'"
          v-model="modelFilter"
          class="w-40 xl:w-52"
          placeholder="Filter actual model..."
          @keyup.enter="refresh()"
        />
        <Button @click="refresh()">
          <Search class="h-3.5 w-3.5" />
          Query
        </Button>
        <RefreshButton :loading="loading" size="default" class="ml-auto" @click="refresh()" />
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
          <div class="overflow-x-auto rounded-lg border">
            <Table class="min-w-max">
              <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
                <TableRow>
                  <TableHead
                    v-for="col in groupColumns"
                    :key="col.key"
                    :class="col.align === 'right' ? 'text-right' : 'text-left'"
                  >
                    {{ col.label }}
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                <TableRow v-for="row in groupRows" :key="row.key">
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
            <div v-else-if="trend" class="space-y-4">
              <V2TrendChart :trend="trend" :scope="scope" />
              <div class="overflow-x-auto">
                <Table class="min-w-max">
                  <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
                    <TableRow>
                      <TableHead class="text-left">Group</TableHead>
                      <TableHead class="text-right">{{ populationLabel(scope) }}</TableHead>
                      <TableHead class="text-right">Success</TableHead>
                      <TableHead class="text-right">Success Rate</TableHead>
                      <TableHead class="text-right">Tokens In</TableHead>
                      <TableHead class="text-right">Tokens Out</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    <TableRow v-for="row in seriesRows" :key="row.label">
                      <TableCell
                        :class="row.others ? 'text-muted-foreground italic' : 'font-medium'"
                      >
                        {{ row.label }}
                      </TableCell>
                      <TableCell class="text-right tabular-nums">
                        {{ row.total.toLocaleString() }}
                      </TableCell>
                      <TableCell class="text-right tabular-nums">
                        {{ row.success.toLocaleString() }}
                      </TableCell>
                      <TableCell class="text-right font-mono text-xs tabular-nums">
                        <span
                          v-if="row.successRate != null"
                          :style="{ color: successRateColor(row.successRate * 100) }"
                        >
                          {{ (row.successRate * 100).toFixed(1) }}%
                        </span>
                        <span v-else class="text-muted-foreground">—</span>
                      </TableCell>
                      <TableCell class="text-right tabular-nums">
                        {{ formatCompactNumber(row.tokensIn) }}
                      </TableCell>
                      <TableCell class="text-right tabular-nums">
                        {{ formatCompactNumber(row.tokensOut) }}
                      </TableCell>
                    </TableRow>
                    <TableRow v-if="seriesRows.length === 0">
                      <TableCell colspan="6" class="text-muted-foreground py-6 text-center">
                        No trend series in this range.
                      </TableCell>
                    </TableRow>
                  </TableBody>
                </Table>
              </div>
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

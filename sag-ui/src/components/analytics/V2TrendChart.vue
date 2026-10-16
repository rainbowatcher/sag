<script setup lang="ts">
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  Filler,
  Tooltip,
  Legend,
} from "chart.js"
import { computed, ref } from "vue"
import { Line } from "vue-chartjs"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { useChartTheme } from "@/composables/useChartTheme"
import { t } from "@/lib/i18n"
import type {
  AnalyticsScope,
  AnalyticsTrendPoint,
  AnalyticsTrendResponse,
  MetricSummary,
} from "@/types/api"
import { groupKeyLabel, populationLabel } from "./v2"

ChartJS.register(CategoryScale, LinearScale, PointElement, LineElement, Filler, Tooltip, Legend)

const props = defineProps<{
  trend: AnalyticsTrendResponse
  scope: AnalyticsScope
}>()

const { chartColors } = useChartTheme()

/**
 * Series drawn as explicit lines. The backend trims to Top-20 + others;
 * drawing all of them would make the legend unreadable, so the chart keeps
 * the overall area + the top few series + the `others` dashed line.
 */
const CHART_SERIES_LIMIT = 8

// ── Switchable metric ──────────────────────────────────────────────
// Per-point metrics are computable for EVERY line (Overall + series +
// others). Overall-only metrics exist only on the overall bucket
// (MetricSummary) — the per-series `AnalyticsTrendPoint` carries no
// cache / latency data — so selecting one hides the series/others lines.
const METRIC_KEYS = [
  "requests",
  "success",
  "success_rate",
  "prompt_tokens",
  "completion_tokens",
  "total_tokens",
  "cache_hit_rate",
  "e2e_avg",
  "e2e_p95",
] as const

type MetricKey = (typeof METRIC_KEYS)[number]

interface MetricOption {
  /** 0..100 percentage scale → y-axis `max: 100`. */
  rate?: boolean
  /** Only the overall bucket has data → hide per-series lines. */
  overallOnly?: boolean
}

const METRICS: Record<MetricKey, MetricOption> = {
  requests: {},
  success: {},
  success_rate: { rate: true },
  prompt_tokens: {},
  completion_tokens: {},
  total_tokens: {},
  cache_hit_rate: { rate: true, overallOnly: true },
  e2e_avg: { overallOnly: true },
  e2e_p95: { overallOnly: true },
}

const metric = ref<MetricKey>("requests")
const metricOption = computed(() => METRICS[metric.value])
const overallOnly = computed(() => metricOption.value.overallOnly === true)

/**
 * Metric labels, resolved through `t()` inside a computed so they re-render
 * on locale switches. The "requests" entry is scope-aware (attempts in
 * target scope are never mislabelled as requests — same wording as the
 * y-axis population label).
 */
const metricLabels = computed<Record<MetricKey, string>>(() => ({
  requests: populationLabel(props.scope),
  success: t("analytics.v2.columns.success"),
  success_rate: t("analytics.v2.metric.successRate"),
  prompt_tokens: t("analytics.v2.metric.promptTokens"),
  completion_tokens: t("analytics.v2.metric.completionTokens"),
  total_tokens: t("analytics.v2.metric.totalTokens"),
  cache_hit_rate: t("analytics.v2.metric.cacheHitRate"),
  e2e_avg: t("analytics.v2.metric.avgE2e"),
  e2e_p95: t("analytics.v2.metric.p95E2e"),
}))

/** Dropdown entries, keyed by metric. */
const metricOptions = computed(() =>
  METRIC_KEYS.map((key) => ({
    key,
    label: metricLabels.value[key],
  })),
)

const labels = computed(() =>
  props.trend.buckets.map((b) =>
    new Date(b.hour_ms).toLocaleString("en-CA", {
      hour: "2-digit",
      minute: "2-digit",
    }),
  ),
)

const population = computed(() => populationLabel(props.scope))

/** Value of a per-series point for the selected metric. When the metric
 *  cannot be computed (e.g. success_rate with zero requests) or there is
 *  no data, it returns `0` as a visual placeholder so the line stays
 *  continuous — a 0 is NOT a real measurement, just a gap filler.
 *  Overall-only metrics return `0` (unreachable for series lines —
 *  guarded by `overallOnly`). */
function pointValue(p: AnalyticsTrendPoint, key: MetricKey): number {
  switch (key) {
    case "requests":
      return p.requests
    case "success":
      return p.success
    case "success_rate":
      return p.requests > 0 ? (p.success / p.requests) * 100 : 0
    case "prompt_tokens":
      return p.prompt_tokens
    case "completion_tokens":
      return p.completion_tokens
    case "total_tokens":
      return p.total_tokens
    default:
      return 0
  }
}

/** Value of the overall bucket for the selected metric. Nullable rates /
 *  latencies map to `0` (visual placeholder, not a fabricated real
 *  measurement) so the line never breaks. */
function overallValue(m: MetricSummary, key: MetricKey, scope: AnalyticsScope): number {
  switch (key) {
    case "requests":
      return scope === "target" ? m.attempts : m.requests
    case "success":
      return m.success
    case "success_rate":
      return m.success_rate === null ? 0 : m.success_rate * 100
    case "prompt_tokens":
      return m.prompt_tokens
    case "completion_tokens":
      return m.completion_tokens
    case "total_tokens":
      return m.total_tokens
    case "cache_hit_rate":
      return m.request_cache_hit_rate === null ? 0 : m.request_cache_hit_rate * 100
    case "e2e_avg":
      return m.e2e.avg_ms ?? 0
    case "e2e_p95":
      return m.e2e.p95_ms ?? 0
  }
}

const chartData = computed(() => {
  const key = metric.value
  const datasets: any[] = [
    {
      label: t("analytics.v2.overallSeries", { population: population.value }),
      data: props.trend.buckets.map((b) => overallValue(b.overall, key, props.scope)),
      borderColor: chartColors.value[0],
      backgroundColor: chartColors.value[0].replace("rgb(", "rgba(").replace(")", ",0.12)"),
      fill: true,
      tension: 0.3,
      pointRadius: 0,
      borderWidth: 2,
    },
  ]
  // Overall-only metrics have no per-series data — draw just the overall line.
  if (!overallOnly.value) {
    props.trend.series.slice(0, CHART_SERIES_LIMIT).forEach((s, i) => {
      datasets.push({
        label: groupKeyLabel(s.key),
        data: s.points.map((p) => pointValue(p, key)),
        borderColor: chartColors.value[(i % (chartColors.value.length - 1)) + 1],
        backgroundColor: "transparent",
        tension: 0.3,
        pointRadius: 0,
        borderWidth: 1.5,
      })
    })
    if (props.trend.others) {
      datasets.push({
        label: t("analytics.v2.othersSeries", {
          count: props.trend.others.total_requests.toLocaleString(),
          population: population.value,
        }),
        data: props.trend.others.points.map((p) => pointValue(p, key)),
        borderColor: "rgb(148 163 184)",
        backgroundColor: "transparent",
        borderDash: [4, 4],
        tension: 0.3,
        pointRadius: 0,
        borderWidth: 1.5,
      })
    }
  }
  return { labels: labels.value, datasets }
})

/** Y-axis title follows the metric (scope-aware wording for `requests`). */
const yTitle = computed(() => metricLabels.value[metric.value])

/** Format a chart tick / tooltip value per the metric family: rates to
 *  1 decimal (+%), latency in rounded ms, counts localized - never raw
 *  floats like 438.4794662427439. */
function formatTick(
  value: string | number | null | undefined,
  rate: boolean | undefined,
  key?: MetricKey,
): string {
  if (value == null || Number.isNaN(Number(value))) return "-"
  const v = Number(value)
  if (rate) return `${v.toFixed(1)}%`
  if (key === "e2e_avg" || key === "e2e_p95") return `${Math.round(v)} ms`
  return v.toLocaleString()
}

const chartOptions = computed(() => ({
  responsive: true,
  maintainAspectRatio: false,
  interaction: { mode: "index" as const, intersect: false },
  scales: {
    y: {
      beginAtZero: true,
      max: metricOption.value.rate ? 100 : undefined,
      title: { display: true, text: yTitle.value },
      ticks: {
        // Keep tick labels free of float artifacts (e.g. 33.333333333).
        callback: (value: string | number) => formatTick(value, metricOption.value.rate),
      },
    },
    x: {
      ticks: { maxTicksLimit: 8 },
    },
  },
  plugins: {
    legend: {
      position: "bottom" as const,
      labels: { boxWidth: 8, boxHeight: 8, font: { size: 10 } },
    },
    tooltip: {
      callbacks: {
        // Format series values per metric (rates to 1 decimal + %, latency
        // in rounded ms, counts localized) instead of raw floats.
        label: (item: any) =>
          ` ${item.dataset.label}: ${formatTick(item.parsed.y, metricOption.value.rate, metric.value)}`,
      },
    },
  },
}))
</script>

<template>
  <div>
    <div class="mb-2 flex items-center gap-2">
      <span class="text-muted-foreground text-xs">{{ $t("analytics.v2.metricLabel") }}</span>
      <Select :model-value="metric" @update:model-value="(v) => (metric = v as MetricKey)">
        <SelectTrigger class="h-7 w-44 text-xs">
          <SelectValue>{{ metricLabels[metric] }}</SelectValue>
        </SelectTrigger>
        <SelectContent class="w-(--reka-select-trigger-width)">
          <SelectItem v-for="m in metricOptions" :key="m.key" :value="m.key">
            {{ m.label }}
          </SelectItem>
        </SelectContent>
      </Select>
      <p v-if="overallOnly" class="text-muted-foreground text-xs">
        {{ $t("analytics.overallOnlyMetric") }}
      </p>
    </div>
    <div class="h-64 xl:h-72">
      <Line :data="chartData" :options="chartOptions" />
    </div>
  </div>
</template>

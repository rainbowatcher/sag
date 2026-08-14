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
import { computed } from "vue"
import { Line } from "vue-chartjs"
import { useChartTheme } from "@/composables/useChartTheme"
import type { AnalyticsScope, AnalyticsTrendResponse } from "@/types/api"
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
 * the overall area + the top few series + the `others` dashed line. The
 * full Top-20 + others list stays available in the trend table.
 */
const CHART_SERIES_LIMIT = 8

const labels = computed(() =>
  props.trend.buckets.map((b) =>
    new Date(b.hour_ms).toLocaleString("en-CA", {
      hour: "2-digit",
      minute: "2-digit",
    }),
  ),
)

const population = computed(() => populationLabel(props.scope))

const chartData = computed(() => {
  const scope = props.scope
  const datasets: any[] = [
    {
      label: `Overall ${population.value}`,
      data: props.trend.buckets.map((b) =>
        scope === "target" ? b.overall.attempts : b.overall.requests,
      ),
      borderColor: chartColors.value[0],
      backgroundColor: chartColors.value[0].replace("rgb(", "rgba(").replace(")", ",0.12)"),
      fill: true,
      tension: 0.3,
      pointRadius: 0,
      borderWidth: 2,
    },
  ]
  props.trend.series.slice(0, CHART_SERIES_LIMIT).forEach((s, i) => {
    datasets.push({
      label: groupKeyLabel(s.key),
      data: s.points.map((p) => p.requests),
      borderColor: chartColors.value[(i % (chartColors.value.length - 1)) + 1],
      backgroundColor: "transparent",
      tension: 0.3,
      pointRadius: 0,
      borderWidth: 1.5,
    })
  })
  if (props.trend.others) {
    datasets.push({
      label: `others (${props.trend.others.total_requests.toLocaleString()} ${population.value})`,
      data: props.trend.others.points.map((p) => p.requests),
      borderColor: "rgb(148 163 184)",
      backgroundColor: "transparent",
      borderDash: [4, 4],
      tension: 0.3,
      pointRadius: 0,
      borderWidth: 1.5,
    })
  }
  return { labels: labels.value, datasets }
})

const chartOptions = computed(() => ({
  responsive: true,
  maintainAspectRatio: false,
  interaction: { mode: "index" as const, intersect: false },
  scales: {
    y: {
      beginAtZero: true,
      title: { display: true, text: population.value },
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
  },
}))
</script>

<template>
  <div class="h-64 xl:h-72">
    <Line :data="chartData" :options="chartOptions" />
  </div>
</template>

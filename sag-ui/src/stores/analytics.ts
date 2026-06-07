import { defineStore } from "pinia"
import { ref, computed } from "vue"
import { api } from "@/lib/client"
import type { StatsRow } from "@/types/api"
import { useProviderStore } from "./providers"
import { useRouteStore } from "./routes"

export const useAnalyticsStore = defineStore("analytics", () => {
  const stats = ref<StatsRow[]>([])
  const statsLoading = ref(false)

  async function fetchStats(provider?: string, model?: string) {
    statsLoading.value = true
    try {
      const params = new URLSearchParams()
      if (provider) params.set("provider", provider)
      if (model) params.set("model", model)
      const { data } = await api.get<{ data: StatsRow[] }>(`/v1/analytics/stats?${params}`)
      stats.value = data.data
    } finally {
      statsLoading.value = false
    }
  }

  const summary = computed(() => {
    const p = useProviderStore()
    const r = useRouteStore()
    return {
      providerCount: p.providers.length,
      routeCount: r.routes.length,
      totalCalls: stats.value.reduce((s, row) => s + row.call_count, 0),
      totalErrors: stats.value.reduce((s, row) => s + row.error_count, 0),
    }
  })

  return {
    stats,
    statsLoading,
    fetchStats,
    summary,
  }
})

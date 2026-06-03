import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import axios from 'axios'

const api = axios.create({ baseURL: '' })

export interface Provider {
  name: string
  provider_type: string
  base_url: string
  priority: number
  max_retries: number
  timeout_secs: number
  supported_params: string[]
}

export interface RouteTarget {
  provider: string
  real_model: string
  priority: number
}

export interface RouteConfig {
  model_name: string
  api_endpoint: string
  targets: RouteTarget[]
}

export interface StatsRow {
  provider: string
  model: string
  call_count: number
  total_tokens: number
  avg_duration_ms: number
  error_count: number
}

export const useApiStore = defineStore('api', () => {
  const apiKey = ref('')

  function headers() {
    return apiKey.value ? { Authorization: `Bearer ${apiKey.value}` } : {}
  }

  // ── Providers ──
  const providers = ref<Provider[]>([])
  const providersLoading = ref(false)

  async function fetchProviders() {
    providersLoading.value = true
    try {
      const { data } = await api.get<{ providers: Provider[] }>('/api/providers', { headers: headers() })
      providers.value = data.providers
    } finally {
      providersLoading.value = false
    }
  }

  async function addProvider(payload: {
    name: string
    api_key: string
    provider_type: string
    base_url?: string
    priority?: number
    supported_params?: string[]
  }) {
    await api.post('/api/providers', payload, { headers: headers() })
  }

  // ── Routes ──
  const routes = ref<RouteConfig[]>([])
  const routesLoading = ref(false)

  async function fetchRoutes() {
    routesLoading.value = true
    try {
      const { data } = await api.get<{ routes: RouteConfig[] }>('/api/routes', { headers: headers() })
      routes.value = data.routes
    } finally {
      routesLoading.value = false
    }
  }

  async function addRoute(providerName: string, payload: {
    model_name: string
    real_model: string
    api_endpoint?: string
    priority?: number
  }) {
    await api.post(`/api/providers/${providerName}/routes`, payload, { headers: headers() })
  }

  // ── Health ──
  const gatewayVersion = ref('')
  const gatewayOnline = ref(false)

  async function checkHealth() {
    try {
      const { data } = await api.get<{ status: string; version: string }>('/health')
      gatewayOnline.value = data.status === 'ok'
      gatewayVersion.value = data.version
    } catch {
      gatewayOnline.value = false
    }
  }

  // ── Analytics ──
  const stats = ref<StatsRow[]>([])
  const statsLoading = ref(false)

  async function fetchStats(provider?: string, model?: string) {
    statsLoading.value = true
    try {
      const params = new URLSearchParams()
      if (provider) params.set('provider', provider)
      if (model) params.set('model', model)
      const { data } = await api.get<{ data: StatsRow[] }>(`/v1/analytics/stats?${params}`)
      stats.value = data.data
    } finally {
      statsLoading.value = false
    }
  }

  // ── Summary ──
  const summary = computed(() => ({
    providerCount: providers.value.length,
    routeCount: routes.value.length,
    totalCalls: stats.value.reduce((s, r) => s + r.call_count, 0),
    totalErrors: stats.value.reduce((s, r) => s + r.error_count, 0),
  }))

  return {
    apiKey,
    providers, providersLoading, fetchProviders, addProvider,
    routes, routesLoading, fetchRoutes, addRoute,
    gatewayVersion, gatewayOnline, checkHealth,
    stats, statsLoading, fetchStats,
    summary,
  }
})

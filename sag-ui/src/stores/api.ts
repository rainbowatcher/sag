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

interface Connection {
  gatewayUrl: string
  apiKey: string
  savedAt: number
}

const LS_KEY = 'sag-connection'
const TTL_MS = 30 * 24 * 60 * 60 * 1000 // 30 days

export const useApiStore = defineStore('api', () => {
  // ═══════════════════════════════════════════════════════════════
  // Connection state (persisted to localStorage)
  // ═══════════════════════════════════════════════════════════════

  const connection = ref<Connection | null>(null)
  const validated = ref(false)
  const connecting = ref(false)
  const connectError = ref('')

  const isConnected = computed(() => connection.value !== null && validated.value)
  const gatewayUrl = computed(() => connection.value?.gatewayUrl ?? '')

  function headers() {
    return connection.value?.apiKey
      ? { Authorization: `Bearer ${connection.value.apiKey}` }
      : {}
  }

  function loadConnection(): boolean {
    const raw = localStorage.getItem(LS_KEY)
    if (!raw) return false

    let conn: Connection
    try {
      conn = JSON.parse(raw) as Connection
    } catch {
      localStorage.removeItem(LS_KEY)
      return false
    }

    if (Date.now() - conn.savedAt > TTL_MS) {
      localStorage.removeItem(LS_KEY)
      return false
    }

    connection.value = conn
    api.defaults.baseURL = conn.gatewayUrl
    return true
  }

  function saveConnection(url: string, key: string) {
    const conn: Connection = { gatewayUrl: url, apiKey: key, savedAt: Date.now() }
    localStorage.setItem(LS_KEY, JSON.stringify(conn))
    connection.value = conn
    api.defaults.baseURL = url
  }

  async function connect(url: string, key: string): Promise<string | null> {
    connecting.value = true
    connectError.value = ''

    // Ensure URL has a protocol
    let normalizedUrl = url
    if (!/^https?:\/\//i.test(normalizedUrl)) {
      normalizedUrl = 'http://' + normalizedUrl
    }

    try {
      // Step 1: Health check (public endpoint)
      const healthApi = axios.create({ baseURL: normalizedUrl })
      let healthOk = false
      try {
        const { data } = await healthApi.get<{ status: string; version: string }>('/health', { timeout: 5000 })
        healthOk = data.status === 'ok'
        if (healthOk) {
          gatewayVersion.value = data.version
          gatewayOnline.value = true
        }
      } catch {
        // continue — will be reported below
      }

      if (!healthOk) {
        const msg = 'Cannot reach gateway. Check the URL and ensure the gateway is running.'
        connectError.value = msg
        return msg
      }

      // Step 2: Auth check — call /api/providers with the key
      const authApi = axios.create({ baseURL: normalizedUrl })
      let authOk = false
      try {
        const { data } = await authApi.get<{ providers: Provider[] }>('/api/providers', {
          headers: { Authorization: `Bearer ${key}` },
          timeout: 5000,
        })
        providers.value = data.providers
        authOk = true
      } catch (e: any) {
        if (e.response?.status === 401) {
          const msg = 'Invalid API key — please check and re-enter.'
          connectError.value = msg
          return msg
        }
        const msg = `Unexpected error: ${e.message}`
        connectError.value = msg
        return msg
      }

      if (!authOk) {
        const msg = 'Auth check failed.'
        connectError.value = msg
        return msg
      }

      // Success
      saveConnection(normalizedUrl, key)
      validated.value = true
      connectError.value = ''

      // Preload routes (awaited to avoid race with dashboard render)
      await fetchRoutes()

      return null // null = success
    } finally {
      connecting.value = false
    }
  }

  function disconnect() {
    localStorage.removeItem(LS_KEY)
    connection.value = null
    validated.value = false
    api.defaults.baseURL = ''
    providers.value = []
    routes.value = []
  }

  const validating = ref(false)

  async function validateConnection(): Promise<boolean> {
    validating.value = true
    try {
      const { data } = await api.get<{ status: string }>('/health', { timeout: 3000 })
      if (data.status === 'ok') {
        validated.value = true
        gatewayOnline.value = true
        gatewayVersion.value = (data as any).version ?? gatewayVersion.value
        return true
      }
    } catch {
      // fall through
    } finally {
      validating.value = false
    }
    disconnect()
    return false
  }

  // ═══════════════════════════════════════════════════════════════
  // 401 handler — redirect to connect screen
  // ═══════════════════════════════════════════════════════════════

  let _router: any = null
  function setRouter(router: any) {
    _router = router
  }

  function handle401() {
    disconnect()
    if (_router) {
      _router.push('/connect?reason=expired')
    }
  }

  // ═══════════════════════════════════════════════════════════════
  // Providers
  // ═══════════════════════════════════════════════════════════════

  const providers = ref<Provider[]>([])
  const providersLoading = ref(false)

  async function fetchProviders() {
    providersLoading.value = true
    try {
      const { data } = await api.get<{ providers: Provider[] }>('/api/providers', { headers: headers() })
      providers.value = data.providers
    } catch (e: any) {
      if (e.response?.status === 401) handle401()
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
    try {
      await api.post('/api/providers', payload, { headers: headers() })
    } catch (e: any) {
      if (e.response?.status === 401) handle401()
      throw e
    }
  }

  // ═══════════════════════════════════════════════════════════════
  // Routes
  // ═══════════════════════════════════════════════════════════════

  const routes = ref<RouteConfig[]>([])
  const routesLoading = ref(false)

  async function fetchRoutes() {
    routesLoading.value = true
    try {
      const { data } = await api.get<{ routes: RouteConfig[] }>('/api/routes', { headers: headers() })
      routes.value = data.routes
    } catch (e: any) {
      if (e.response?.status === 401) handle401()
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
    try {
      await api.post(`/api/providers/${providerName}/routes`, payload, { headers: headers() })
    } catch (e: any) {
      if (e.response?.status === 401) handle401()
      throw e
    }
  }

  // ═══════════════════════════════════════════════════════════════
  // Health
  // ═══════════════════════════════════════════════════════════════

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

  // ═══════════════════════════════════════════════════════════════
  // Analytics
  // ═══════════════════════════════════════════════════════════════

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

  // ═══════════════════════════════════════════════════════════════
  // Summary
  // ═══════════════════════════════════════════════════════════════

  const summary = computed(() => ({
    providerCount: providers.value.length,
    routeCount: routes.value.length,
    totalCalls: stats.value.reduce((s, r) => s + r.call_count, 0),
    totalErrors: stats.value.reduce((s, r) => s + r.error_count, 0),
  }))

  return {
    // Connection
    connection, validated, connecting, validating, connectError,
    isConnected, gatewayUrl,
    loadConnection, connect, disconnect, validateConnection,
    setRouter,
    // Providers
    providers, providersLoading, fetchProviders, addProvider,
    // Routes
    routes, routesLoading, fetchRoutes, addRoute,
    // Health
    gatewayVersion, gatewayOnline, checkHealth,
    // Analytics
    stats, statsLoading, fetchStats,
    // Summary
    summary,
  }
})

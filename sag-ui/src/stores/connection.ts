import { useLocalStorage, StorageSerializers } from "@vueuse/core"
import axios from "axios"
import { defineStore } from "pinia"
import { ref, computed } from "vue"
import { api } from "@/lib/client"

interface Connection {
  gatewayUrl: string
  apiKey: string
  savedAt: number
}

const LS_KEY = "sag-connection"
const TTL_MS = 30 * 24 * 60 * 60 * 1000

export const useConnectionStore = defineStore("connection", () => {
  const connection = useLocalStorage<Connection | null>(LS_KEY, null, {
    serializer: StorageSerializers.object,
  })
  const validated = ref(false)
  const connecting = ref(false)
  const connectError = ref("")
  const validating = ref(false)

  const isConnected = computed(() => connection.value !== null && validated.value)
  const gatewayUrl = computed(() => connection.value?.gatewayUrl ?? "")

  let _router: any = null

  function setRouter(router: any) {
    _router = router
  }

  /** Keep the global axios Authorization header in sync with the saved key. */
  function setBearer(key: string | null | undefined) {
    if (key) {
      api.defaults.headers.common.Authorization = `Bearer ${key}`
    } else {
      delete api.defaults.headers.common.Authorization
    }
  }

  /** Extract origin from a URL string (scheme + host + port). */
  function getOrigin(url: string): string {
    try {
      const u = new URL(url)
      return u.origin
    } catch {
      return ""
    }
  }

  /** Same-origin gating: return empty string for relative baseURL to
   *  bypass browser CORS preflight entirely.
   *  Named `resolveEmbeddedBaseURL` to signal the assumption that the
   *  gateway serves at root (rust-embed), not under a path prefix. */
  function resolveEmbeddedBaseURL(url: string): string {
    return getOrigin(url) === window.location.origin ? "" : url
  }

  function loadConnection(): boolean {
    if (!connection.value) {
      localStorage.removeItem(LS_KEY)
      return false
    }

    if (Date.now() - connection.value.savedAt > TTL_MS) {
      connection.value = null
      return false
    }

    api.defaults.baseURL = resolveEmbeddedBaseURL(connection.value.gatewayUrl)
    setBearer(connection.value.apiKey)
    return true
  }

  function saveConnection(url: string, key: string) {
    connection.value = { gatewayUrl: url, apiKey: key, savedAt: Date.now() }
    api.defaults.baseURL = resolveEmbeddedBaseURL(url)
    setBearer(key)
  }

  async function connect(url: string, key: string): Promise<string | null> {
    connecting.value = true
    connectError.value = ""

    let normalizedUrl = url
    if (!/^https?:\/\//i.test(normalizedUrl)) {
      normalizedUrl = "http://" + normalizedUrl
    }

    try {
      const healthApi = axios.create({ baseURL: normalizedUrl })
      let healthOk = false
      try {
        const { data } = await healthApi.get<{ status: string; version: string }>("/health", {
          timeout: 5000,
        })
        healthOk = data.status === "ok"
        if (healthOk) {
          const { useHealthStore } = await import("./health")
          useHealthStore().$patch({ gatewayVersion: data.version, gatewayOnline: true })
        }
      } catch {
        // continue
      }

      if (!healthOk) {
        const msg = "Cannot reach gateway. Check the URL and ensure the gateway is running."
        connectError.value = msg
        return msg
      }

      const authApi = axios.create({ baseURL: normalizedUrl })
      try {
        const { data } = await authApi.get<{ providers: any[] }>("/api/providers", {
          headers: { Authorization: `Bearer ${key}` },
          timeout: 5000,
        })
        const { useProviderStore } = await import("./providers")
        useProviderStore().providers = data.providers
      } catch (e: any) {
        if (e.response?.status === 401) {
          const msg = "Invalid API key — please check and re-enter."
          connectError.value = msg
          return msg
        }
        const msg = `Unexpected error: ${e.message}`
        connectError.value = msg
        return msg
      }

      saveConnection(normalizedUrl, key)
      validated.value = true
      connectError.value = ""

      const { useRouteStore } = await import("./routes")
      await useRouteStore().fetchRoutes()

      return null
    } finally {
      connecting.value = false
    }
  }

  function disconnect() {
    connection.value = null
    validated.value = false
    api.defaults.baseURL = ""
    setBearer(null)
  }

  async function handle401() {
    connection.value = null
    validated.value = false
    api.defaults.baseURL = ""
    setBearer(null)
    if (_router) {
      _router.push("/connect?reason=expired")
    }
  }

  async function validateConnection(): Promise<boolean> {
    validating.value = true
    try {
      const { data } = await api.get<{ status: string }>("/health", { timeout: 3000 })
      if (data.status === "ok") {
        validated.value = true
        const { useHealthStore } = await import("./health")
        useHealthStore().$patch({
          gatewayOnline: true,
          gatewayVersion: (data as any).version ?? useHealthStore().gatewayVersion,
        })
        return true
      }
    } catch {
      const { useHealthStore } = await import("./health")
      useHealthStore().gatewayOnline = false
      return false
    } finally {
      validating.value = false
    }
    return false
  }

  return {
    connection,
    validated,
    connecting,
    validating,
    connectError,
    isConnected,
    gatewayUrl,
    loadConnection,
    connect,
    disconnect,
    validateConnection,
    setRouter,
    handle401,
  }
})

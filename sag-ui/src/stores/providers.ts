import { defineStore } from "pinia"
import { ref } from "vue"
import { api, headers } from "@/lib/client"
import type { ModelTestResult, Provider } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useProviderStore = defineStore("providers", () => {
  const providers = ref<Provider[]>([])
  const providersLoading = ref(false)

  async function fetchProviders() {
    providersLoading.value = true
    try {
      const conn = useConnectionStore()
      const { data } = await api.get<{ providers: Provider[] }>("/api/providers", {
        headers: headers(conn.connection?.apiKey),
      })
      providers.value = data.providers
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
    } finally {
      providersLoading.value = false
    }
  }

  async function testModel(payload: {
    model: string
    api_keys: string[]
    base_url: string
    provider_type?: string
    headers?: Record<string, string>
  }): Promise<ModelTestResult[]> {
    const conn = useConnectionStore()
    const { data } = await api.post<{ results: ModelTestResult[] }>("/api/model/test", payload, {
      headers: headers(conn.connection?.apiKey),
    })
    return data.results
  }

  async function addProvider(payload: {
    name: string
    api_key: string
    api_keys?: string[]
    key_strategy?: string
    provider_type: string
    base_url?: string
    priority?: number
    max_retries?: number
    timeout_secs?: number
    headers?: Record<string, string>
    body_overrides?: Record<string, any>
    enabled_models?: string[]
  }) {
    try {
      const conn = useConnectionStore()
      await api.post("/api/providers", payload, { headers: headers(conn.connection?.apiKey) })
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function deleteProvider(name: string): Promise<void> {
    try {
      const conn = useConnectionStore()
      await api.delete("/api/providers", {
        headers: headers(conn.connection?.apiKey),
        data: { name },
      })
      const { useRouteStore } = await import("./routes")
      await Promise.all([fetchProviders(), useRouteStore().fetchRoutes()])
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function updateProvider(
    name: string,
    payload: {
      api_key?: string
      api_keys?: string[]
      key_strategy?: string
      provider_type?: string
      base_url?: string
      priority?: number
      max_retries?: number
      timeout_secs?: number
      headers?: Record<string, string>
      body_overrides?: Record<string, any>
      enabled_models?: string[]
    },
  ) {
    try {
      const conn = useConnectionStore()
      await api.put(
        "/api/providers",
        { name, ...payload },
        { headers: headers(conn.connection?.apiKey) },
      )
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  return {
    providers,
    providersLoading,
    fetchProviders,
    addProvider,
    deleteProvider,
    updateProvider,
    testModel,
  }
})

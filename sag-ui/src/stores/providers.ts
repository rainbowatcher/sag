import { defineStore } from "pinia"
import { ref } from "vue"
import { api, headers } from "@/lib/client"
import type { ModelTestResult, Provider } from "@/types/api"
import { useConnectionStore } from "./connection"

function createPerItemState<T, K extends string>() {
  const states = ref<
    Record<string, { loading: boolean; error: string; data: T | undefined } | undefined>
  >({})
  async function execute(key: K, fn: () => Promise<T>): Promise<T | undefined> {
    states.value[key] = { loading: true, error: "", data: undefined }
    try {
      const result = await fn()
      states.value[key] = { loading: false, error: "", data: result }
      return result
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      const errorMsg = e.response?.data?.error?.message ?? e.message ?? String(e)
      states.value[key] = { loading: false, error: errorMsg, data: undefined }
    }
  }
  return { states, execute }
}

export const useProviderStore = defineStore("providers", () => {
  const providers = ref<Provider[]>([])
  const providersLoading = ref(false)

  const v1Models = ref<Record<string, unknown>[]>([])
  const v1ModelsLoading = ref(false)
  const v1ModelsError = ref<string>("")

  const testModelState = createPerItemState<ModelTestResult[], string>()

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

  async function fetchUpstreamModels(payload: {
    base_url: string
    api_keys: string[]
    headers?: Record<string, string>
    provider_type?: string
    models_endpoint?: string
  }) {
    v1ModelsLoading.value = true
    v1ModelsError.value = ""
    try {
      const conn = useConnectionStore()
      const { data } = await api.post<{ models: Record<string, unknown>[] }>(
        "/api/providers/fetch-models",
        payload,
        {
          headers: headers(conn.connection?.apiKey),
        },
      )
      v1Models.value = data.models
      v1ModelsError.value = ""
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      v1ModelsError.value = e.response?.data?.error?.message ?? e.message ?? String(e)
    } finally {
      v1ModelsLoading.value = false
    }
  }

  async function testModelWithState(
    model: string,
    payload: {
      model: string
      api_keys: string[]
      base_url: string
      provider_type?: string
      headers?: Record<string, string>
    },
  ) {
    return await testModelState.execute(model, async () => {
      const conn = useConnectionStore()
      const { data } = await api.post<{ results: ModelTestResult[] }>("/api/model/test", payload, {
        headers: headers(conn.connection?.apiKey),
      })
      return data.results
    })
  }

  async function addProvider(payload: {
    name: string
    api_keys: string[]
    key_strategy?: string
    provider_type: string
    base_url?: string
    priority?: number
    max_retries?: number
    timeout_secs?: number
    headers?: Record<string, string>
    body_overrides?: Record<string, unknown>
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
    const oldProviders = [...providers.value]
    providers.value = providers.value.filter((p) => p.name !== name)
    try {
      const conn = useConnectionStore()
      await api.delete("/api/providers", {
        headers: headers(conn.connection?.apiKey),
        data: { name },
      })
      const { useRouteStore } = await import("./routes")
      await Promise.all([fetchProviders(), useRouteStore().fetchRoutes()])
    } catch (e: any) {
      providers.value = oldProviders
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function updateProvider(
    name: string,
    payload: {
      api_keys?: string[]
      key_strategy?: string
      provider_type?: string
      base_url?: string
      priority?: number
      max_retries?: number
      timeout_secs?: number
      headers?: Record<string, string>
      body_overrides?: Record<string, unknown>
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
      await fetchProviders()
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  return {
    providers,
    providersLoading,
    v1Models,
    v1ModelsLoading,
    v1ModelsError,
    fetchProviders,
    fetchUpstreamModels,
    addProvider,
    deleteProvider,
    updateProvider,
    testModelState,
    testModelWithState,
  }
})

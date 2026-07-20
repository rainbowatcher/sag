import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { ModelTestResult, Provider } from "@/types/api"
import { useConnectionStore } from "./connection"

export interface FetchProvidersParams {
  model_name?: string
  provider_type?: string
  sort?: string
}

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
      const errorMsg = getErrorMessage(e, String(e))
      states.value[key] = { loading: false, error: errorMsg, data: undefined }
    }
  }
  return { states, execute }
}

export const useProviderStore = defineStore("providers", () => {
  const providers = ref<Provider[]>([])
  const providersLoading = ref(false)

  const upstreamModels = ref<Record<string, unknown>[]>([])
  const upstreamModelsLoading = ref(false)
  const upstreamModelsError = ref<string>("")

  const allProviderModels = ref<string[]>([])
  const allProviderModelsLoading = ref(false)

  const testModelState = createPerItemState<ModelTestResult[], string>()

  async function fetchProviders(params: FetchProvidersParams = {}) {
    providersLoading.value = true
    try {
      const queryParams = new URLSearchParams()
      if (params.model_name?.trim()) queryParams.set("model_name", params.model_name.trim())
      if (params.provider_type) queryParams.set("provider_type", params.provider_type)
      if (params.sort) queryParams.set("sort", params.sort)
      const query = queryParams.toString()
      const { data } = await api.get<{ providers: Provider[] }>(
        `/api/providers${query ? `?${query}` : ""}`,
      )
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
    upstreamModelsLoading.value = true
    upstreamModelsError.value = ""
    try {
      const { data } = await api.post<{ models: Record<string, unknown>[] }>(
        "/api/providers/fetch-models",
        payload,
      )
      upstreamModels.value = data.models
      upstreamModelsError.value = ""
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      upstreamModelsError.value = getErrorMessage(e, String(e))
    } finally {
      upstreamModelsLoading.value = false
    }
  }

  async function fetchAllProviderModels() {
    allProviderModelsLoading.value = true
    try {
      const { data } = await api.get<{ providers: Provider[] }>("/api/providers?sort=name")
      const models = new Set<string>()
      for (const p of data.providers) {
        for (const m of p.enabled_models) {
          models.add(m)
        }
      }
      allProviderModels.value = Array.from(models).sort()
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
    } finally {
      allProviderModelsLoading.value = false
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
      const { data } = await api.post<{ results: ModelTestResult[] }>("/api/model/test", payload)
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
    model_overrides?: Record<string, Record<string, unknown>>
    enabled_models?: string[]
    models_endpoint?: string
  }) {
    try {
      await api.post("/api/providers", payload)
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function deleteProvider(name: string): Promise<void> {
    const oldProviders = [...providers.value]
    providers.value = providers.value.filter((p) => p.name !== name)
    try {
      await api.delete("/api/providers", {
        data: { name },
      })
      const { useRouteStore } = await import("./routes")
      await useRouteStore().fetchRoutes()
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
      models_endpoint?: string
      priority?: number
      max_retries?: number
      timeout_secs?: number
      headers?: Record<string, string>
      body_overrides?: Record<string, unknown>
      model_overrides?: Record<string, Record<string, unknown>>
      enabled_models?: string[]
      enabled?: boolean
    },
  ) {
    try {
      await api.put("/api/providers", { name, ...payload })
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      console.error("[updateProvider] error:", e.code, e.message, e.response?.data)
      throw e
    }
  }

  return {
    providers,
    providersLoading,
    upstreamModels,
    upstreamModelsLoading,
    upstreamModelsError,
    allProviderModels,
    allProviderModelsLoading,
    fetchProviders,
    fetchUpstreamModels,
    fetchAllProviderModels,
    addProvider,
    deleteProvider,
    updateProvider,
    testModelState,
    testModelWithState,
  }
})

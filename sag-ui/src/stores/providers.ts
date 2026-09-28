import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type {
  AddProviderPayload,
  ApiKind,
  ModelTestResult,
  ProxyMode,
  Provider,
  UpdateProviderPayload,
} from "@/types/api"
import { useConnectionStore } from "./connection"

export interface FetchProvidersParams {
  model_name?: string
  /** Only keep providers that declare this `apis` section. */
  apis?: ApiKind
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
      if (params.apis) queryParams.set("apis", params.apis)
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
    /** Probe address: the `base_url` of the `apis.<api_kind>` section under test. */
    base_url: string
    api_keys: string[]
    headers?: Record<string, string>
    /** API section whose auth style applies (default `chat_completions`). */
    api_kind?: ApiKind
    models_endpoint?: string
    proxy_mode?: ProxyMode
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
      /** Probe address: the `base_url` of the `apis.<api_kind>` section under test. */
      base_url: string
      api_kind?: ApiKind
      headers?: Record<string, string>
      proxy_mode?: ProxyMode
    },
  ) {
    return await testModelState.execute(model, async () => {
      const { data } = await api.post<{ results: ModelTestResult[] }>("/api/model/test", payload)
      return data.results
    })
  }

  async function addProvider(payload: AddProviderPayload) {
    try {
      await api.post("/api/providers", payload)
      await fetchProviders()
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
      const routeStore = useRouteStore()
      routeStore.clearProviderModelsCache(name)
      await routeStore.fetchRoutes()
    } catch (e: any) {
      providers.value = oldProviders
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function updateProvider(name: string, payload: Omit<UpdateProviderPayload, "name">) {
    try {
      await api.put("/api/providers", { name, ...payload })
      await fetchProviders()
      const { useRouteStore } = await import("./routes")
      const routeStore = useRouteStore()
      routeStore.clearProviderModelsCache(name)
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

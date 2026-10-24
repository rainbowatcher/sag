import { defineStore } from "pinia"
import { computed, ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { Provider, RouteConfig, CreateRoutePayload, UpdateRoutePayload } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useRouteStore = defineStore("routes", () => {
  const routes = ref<RouteConfig[]>([])
  const routesLoading = ref(false)
  const providerModels = ref<Record<string, string[]>>({})
  /**
   * Number of `/models` probes currently in flight. The loading flag is derived
   * from it so concurrent probes (the target picker loads every provider at
   * once) cannot clear it while a sibling request is still pending.
   */
  const providerModelsPending = ref(0)
  const providerModelsLoading = computed(() => providerModelsPending.value > 0)
  /** Per-provider failure messages from the last `/models` probe. */
  const providerModelsErrors = ref<Record<string, string>>({})

  async function fetchRoutes() {
    routesLoading.value = true
    try {
      const { data } = await api.get<{ routes: RouteConfig[] }>("/api/routes")
      routes.value = data.routes
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
    } finally {
      routesLoading.value = false
    }
  }

  async function fetchProviderModels(name: string) {
    providerModelsPending.value += 1
    try {
      const { data } = await api.get<{ models: string[] }>(
        `/api/providers/${encodeURIComponent(name)}/models`,
      )
      providerModels.value[name] = data.models
      delete providerModelsErrors.value[name]
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      providerModels.value[name] = []
      providerModelsErrors.value[name] = getErrorMessage(e, String(e))
    } finally {
      providerModelsPending.value = Math.max(0, providerModelsPending.value - 1)
    }
  }

  function clearProviderModelsCache(providerName?: string) {
    if (providerName) {
      delete providerModels.value[providerName]
      delete providerModelsErrors.value[providerName]
    } else {
      providerModels.value = {}
      providerModelsErrors.value = {}
    }
  }

  /**
   * Resolve one provider's model list: the curated `enabled_models` when the
   * provider declares one (synchronous), otherwise the upstream `/models`
   * probe. Always awaits the probe so callers never depend on that split.
   */
  async function loadProviderModels(providerName: string, providers: Provider[]) {
    const p = providers.find((p) => p.name === providerName)
    if (p && p.enabled_models.length > 0) {
      providerModels.value[providerName] = p.enabled_models
    } else if (providerName) {
      await fetchProviderModels(providerName)
    }
  }

  async function createRoute(payload: CreateRoutePayload) {
    try {
      await api.post("/api/routes", payload)
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function updateRoute(payload: UpdateRoutePayload) {
    try {
      await api.put("/api/routes", payload)
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function deleteRoute(modelName: string) {
    try {
      await api.delete("/api/routes", {
        data: { model_name: modelName },
      })
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  return {
    routes,
    routesLoading,
    providerModels,
    providerModelsLoading,
    providerModelsErrors,
    fetchRoutes,
    loadProviderModels,
    fetchProviderModels,
    clearProviderModelsCache,
    createRoute,
    updateRoute,
    deleteRoute,
  }
})

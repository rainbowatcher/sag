import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import type { Provider, RouteConfig, CreateRoutePayload, UpdateRoutePayload } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useRouteStore = defineStore("routes", () => {
  const routes = ref<RouteConfig[]>([])
  const routesLoading = ref(false)
  const providerModels = ref<Record<string, string[]>>({})
  const providerModelsLoading = ref(false)

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
    providerModelsLoading.value = true
    try {
      const { data } = await api.get<{ models: string[] }>(
        `/api/providers/${encodeURIComponent(name)}/models`,
      )
      providerModels.value[name] = data.models
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      providerModels.value[name] = []
    } finally {
      providerModelsLoading.value = false
    }
  }

  function loadProviderModels(providerName: string, providers: Provider[]) {
    const p = providers.find((p) => p.name === providerName)
    if (p && p.enabled_models.length > 0) {
      providerModels.value[providerName] = p.enabled_models
    } else if (providerName) {
      fetchProviderModels(providerName)
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
    fetchRoutes,
    loadProviderModels,
    fetchProviderModels,
    createRoute,
    updateRoute,
    deleteRoute,
  }
})

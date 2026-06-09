import { defineStore } from "pinia"
import { ref } from "vue"
import { api, headers } from "@/lib/client"
import type { RouteConfig, CreateRoutePayload, UpdateRoutePayload } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useRouteStore = defineStore("routes", () => {
  const routes = ref<RouteConfig[]>([])
  const routesLoading = ref(false)
  const providerModels = ref<Record<string, string[]>>({})
  const providerModelsLoading = ref(false)

  async function fetchRoutes() {
    routesLoading.value = true
    try {
      const conn = useConnectionStore()
      const { data } = await api.get<{ routes: RouteConfig[] }>("/api/routes", {
        headers: headers(conn.connection?.apiKey),
      })
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
      const conn = useConnectionStore()
      const { data } = await api.get<{ models: string[] }>(
        `/api/providers/${encodeURIComponent(name)}/models`,
        { headers: headers(conn.connection?.apiKey) },
      )
      providerModels.value[name] = data.models
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      providerModels.value[name] = []
    } finally {
      providerModelsLoading.value = false
    }
  }

  async function createRoute(payload: CreateRoutePayload) {
    try {
      const conn = useConnectionStore()
      await api.post("/api/routes", payload, {
        headers: headers(conn.connection?.apiKey),
      })
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function updateRoute(payload: UpdateRoutePayload) {
    try {
      const conn = useConnectionStore()
      await api.put("/api/routes", payload, {
        headers: headers(conn.connection?.apiKey),
      })
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  async function deleteRoute(modelName: string) {
    try {
      const conn = useConnectionStore()
      await api.delete("/api/routes", {
        headers: headers(conn.connection?.apiKey),
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
    fetchProviderModels,
    createRoute,
    updateRoute,
    deleteRoute,
  }
})

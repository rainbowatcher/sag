import { defineStore } from "pinia"
import { ref } from "vue"
import { api, headers } from "@/lib/client"
import type { RouteConfig, AddRoutePayload } from "@/types/api"
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

  async function addRoute(providerName: string, payload: AddRoutePayload) {
    try {
      const conn = useConnectionStore()
      await api.post(`/api/providers/${providerName}/routes`, payload, {
        headers: headers(conn.connection?.apiKey),
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
    addRoute,
  }
})

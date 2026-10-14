import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { GatewayConfigSnapshot, UpdateGatewayConfigPayload } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useGatewayConfigStore = defineStore("gatewayConfig", () => {
  /** Full gateway config snapshot; `null` until the first successful load. */
  const config = ref<GatewayConfigSnapshot | null>(null)
  const loading = ref(false)
  const saving = ref(false)
  const error = ref("")

  /** Load the full gateway config from `GET /api/config`. */
  async function fetchConfig() {
    loading.value = true
    error.value = ""
    try {
      const { data } = await api.get<GatewayConfigSnapshot>("/api/config")
      config.value = data
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, String(e))
    } finally {
      loading.value = false
    }
  }

  /**
   * Persist runtime-tuning fields via `PUT /api/config`; omitted fields stay
   * unchanged. Refreshes the snapshot from the response and returns it.
   */
  async function saveRuntimeConfig(payload: UpdateGatewayConfigPayload) {
    saving.value = true
    error.value = ""
    try {
      const { data } = await api.put<GatewayConfigSnapshot>("/api/config", payload)
      config.value = data
      return data
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, String(e))
      throw e
    } finally {
      saving.value = false
    }
  }

  return {
    config,
    loading,
    saving,
    error,
    fetchConfig,
    saveRuntimeConfig,
  }
})

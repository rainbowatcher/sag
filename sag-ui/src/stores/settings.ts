import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { Settings, UpdateSettingsPayload } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useSettingsStore = defineStore("settings", () => {
  /** Current gateway-level proxy URL; `null` when none is configured. */
  const proxyUrl = ref<string | null>(null)
  const loading = ref(false)
  const saving = ref(false)
  const error = ref("")

  /** Load current settings from `GET /api/settings`. */
  async function fetchSettings() {
    loading.value = true
    error.value = ""
    try {
      const { data } = await api.get<Settings>("/api/settings")
      proxyUrl.value = data.proxy_url ?? null
    } catch (e: any) {
      if (e.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, String(e))
    } finally {
      loading.value = false
    }
  }

  /** Persist settings via `PUT /api/settings`; returns the saved settings. */
  async function saveSettings(payload: UpdateSettingsPayload) {
    saving.value = true
    error.value = ""
    try {
      const { data } = await api.put<Settings>("/api/settings", payload)
      proxyUrl.value = data.proxy_url ?? null
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
    proxyUrl,
    loading,
    saving,
    error,
    fetchSettings,
    saveSettings,
  }
})

import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { ActiveTargetEntry } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useActiveTargetStore = defineStore("activeTarget", () => {
  const targets = ref<ActiveTargetEntry[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchActiveTargets() {
    loading.value = true
    error.value = null
    try {
      const { data } = await api.get<{ targets: ActiveTargetEntry[] }>("/api/active-targets")
      targets.value = data.targets
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, "Failed to load active target state")
    } finally {
      loading.value = false
    }
  }

  async function clearActiveTarget(model: string) {
    try {
      await api.delete("/api/active-targets", { data: { model } })
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
    // Refresh after clearing so the UI reflects the updated state.
    await fetchActiveTargets()
  }

  return {
    targets,
    loading,
    error,
    fetchActiveTargets,
    clearActiveTarget,
  }
})

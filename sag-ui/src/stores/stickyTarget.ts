import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { StickyTargetEntry } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useStickyTargetStore = defineStore("stickyTarget", () => {
  const targets = ref<StickyTargetEntry[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchStickyTargets() {
    loading.value = true
    error.value = null
    try {
      const { data } = await api.get<{ targets: StickyTargetEntry[] }>("/api/sticky-targets")
      targets.value = data.targets
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, "Failed to load sticky target state")
    } finally {
      loading.value = false
    }
  }

  async function clearStickyTarget(model: string) {
    try {
      await api.delete("/api/sticky-targets", { data: { model } })
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
    // Refresh after clearing so the UI reflects the updated state.
    await fetchStickyTargets()
  }

  return {
    targets,
    loading,
    error,
    fetchStickyTargets,
    clearStickyTarget,
  }
})

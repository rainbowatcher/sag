import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { SessionAffinityDeletedResult, SessionAffinityEntry } from "@/types/api"
import { useConnectionStore } from "./connection"

/** Query filter for `GET /api/session-affinity` (all fields optional). */
export interface SessionAffinityQuery {
  model?: string
  key?: string
  /** Page size; backend default 200, clamped to 1000. */
  limit?: number
}

/**
 * Live session-affinity (session_sticky) pins, mirroring
 * `GET|DELETE /api/session-affinity`.
 *
 * Deletes do not refetch by themselves: the caller knows which scope it
 * displayed (one key vs. one model space), so the popover reloads that scope
 * after a mutation.
 */
export const useSessionAffinityStore = defineStore("sessionAffinity", () => {
  const entries = ref<SessionAffinityEntry[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  // Monotonic token: only the latest in-flight fetch may write state, so a
  // slow earlier request cannot overwrite a newer one.
  let generation = 0

  async function fetchEntries(params: SessionAffinityQuery = {}) {
    const token = ++generation
    loading.value = true
    error.value = null
    try {
      const { data } = await api.get<{ entries: SessionAffinityEntry[] }>("/api/session-affinity", {
        params,
      })
      if (token !== generation) return
      entries.value = data.entries
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      if (token !== generation) return
      entries.value = []
      error.value = getErrorMessage(e, "Failed to load session pins")
    } finally {
      if (token === generation) loading.value = false
    }
  }

  /** Clear one session (across every model space) by its hashed key. */
  async function deleteByKey(key: string): Promise<SessionAffinityDeletedResult> {
    try {
      const { data } = await api.delete<SessionAffinityDeletedResult>("/api/session-affinity", {
        data: { key },
      })
      return data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  /** Clear every session pin of one request-model space. */
  async function deleteByModel(model: string): Promise<SessionAffinityDeletedResult> {
    try {
      const { data } = await api.delete<SessionAffinityDeletedResult>("/api/session-affinity", {
        data: { model },
      })
      return data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
  }

  return {
    entries,
    loading,
    error,
    fetchEntries,
    deleteByKey,
    deleteByModel,
  }
})

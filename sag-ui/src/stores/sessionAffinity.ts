import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { mergeSessionEntries, type SessionAffinityError } from "@/lib/sessionAffinity"
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
  // The store is shared by every route row, so a failure is tagged with the
  // model space it belongs to: a failed `{ model: B }` fetch must not turn
  // row A's list into B's error text.
  const error = ref<SessionAffinityError | null>(null)

  // Monotonic token: only the latest in-flight fetch may write state, so a
  // slow earlier request cannot overwrite a newer one.
  let generation = 0

  /** Drop the stored failure only when it belongs to the scope being fetched. */
  function clearError(scope?: string) {
    if (error.value !== null && error.value.scope === scope) error.value = null
  }

  async function fetchEntries(params: SessionAffinityQuery = {}) {
    const token = ++generation
    const scope = params.model
    loading.value = true
    clearError(scope)
    try {
      const { data } = await api.get<{ entries: SessionAffinityEntry[] }>("/api/session-affinity", {
        params,
      })
      if (token !== generation) return
      // A `{ model }` fetch only owns its own slice of the shared list; the other
      // rows' pins must survive so they do not render as a false "no pins".
      entries.value = mergeSessionEntries(entries.value, data.entries, scope)
      clearError(scope)
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      if (token !== generation) return
      // A failed fetch is not an empty result: keep the last successful data so
      // the other rows (and this one) do not render as a false "no pins". The
      // scoped `error` is what tells this row's caller the refresh failed.
      error.value = { scope, message: getErrorMessage(e, "Failed to load session pins") }
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

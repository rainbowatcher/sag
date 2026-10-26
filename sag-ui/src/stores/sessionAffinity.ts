import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"
import { mergeSessionEntries } from "@/lib/sessionAffinity"
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
 * Key of one fetch scope: the request-model space it asked for, `""` for a
 * full-space fetch. Every per-scope piece of state (in-flight flag, failure,
 * generation token) is keyed by this so rows never overwrite each other.
 */
function scopeKeyOf(params: SessionAffinityQuery): string {
  return params.model ?? ""
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
  // The store is shared by every route row, so the in-flight flag and the
  // failure are per scope: one row's fetch must neither put another row into a
  // spinner nor render its error text.
  const loading = ref<Record<string, boolean>>({})
  const errors = ref<Record<string, string>>({})

  // One monotonic token per scope: only the latest in-flight fetch of a scope
  // may write that scope's state, so a slow earlier request cannot overwrite a
  // newer one. A stale response skips its own scope's writes only.
  const generations = new Map<string, number>()

  function isCurrent(scope: string, token: number): boolean {
    return generations.get(scope) === token
  }

  /** Drop the stored failure of one scope only (`""` = the full-space fetch). */
  function clearError(scope = "") {
    if (!Object.hasOwn(errors.value, scope)) return
    const { [scope]: _dropped, ...rest } = errors.value
    errors.value = rest
  }

  async function fetchEntries(params: SessionAffinityQuery = {}) {
    const scope = scopeKeyOf(params)
    const token = (generations.get(scope) ?? 0) + 1
    generations.set(scope, token)
    loading.value = { ...loading.value, [scope]: true }
    // A retry keeps the previous failure visible until it succeeds or fails
    // again: clearing it here flashed the loading/empty state of this row.
    try {
      const { data } = await api.get<{ entries: SessionAffinityEntry[] }>("/api/session-affinity", {
        params,
      })
      if (!isCurrent(scope, token)) return
      // A `{ model }` fetch only owns its own slice of the shared list; the other
      // rows' pins must survive so they do not render as a false "no pins".
      entries.value = mergeSessionEntries(entries.value, data.entries, params.model)
      clearError(scope)
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      if (!isCurrent(scope, token)) return
      // A failed fetch is not an empty result: keep the last successful data so
      // the other rows (and this one) do not render as a false "no pins". The
      // scoped `errors[scope]` is what tells this row's caller the refresh
      // failed, without taking another row's message away.
      errors.value = {
        ...errors.value,
        [scope]: getErrorMessage(e, "Failed to load session pins"),
      }
    } finally {
      if (isCurrent(scope, token)) {
        const { [scope]: _done, ...rest } = loading.value
        loading.value = rest
      }
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
    errors,
    fetchEntries,
    deleteByKey,
    deleteByModel,
  }
})

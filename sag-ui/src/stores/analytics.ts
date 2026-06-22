import { defineStore } from "pinia"
import { ref, computed } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { CallLog, LogFilter, StatsRow } from "@/types/api"
import { useConnectionStore } from "./connection"
import { useProviderStore } from "./providers"
import { useRouteStore } from "./routes"

/** Default page size for the logs tab; matches `DEFAULT_LOG_LIMIT` on the server. */
const DEFAULT_LOG_LIMIT = 50

export const useAnalyticsStore = defineStore("analytics", () => {
  // ── Stats (aggregated) ────────────────────────────────────────────
  const stats = ref<StatsRow[]>([])
  const statsLoading = ref(false)
  const statsError = ref<string | null>(null)

  async function fetchStats(provider?: string, model?: string) {
    statsLoading.value = true
    statsError.value = null
    try {
      const params = new URLSearchParams()
      if (provider) params.set("provider", provider)
      if (model) params.set("model", model)
      // No per-call headers — `connection.ts:saveConnection` keeps
      // `api.defaults.headers.common.Authorization` in sync with the
      // persisted apiKey (Step 1 A2). `scripts/lint-api-auth.mjs` flags
      // any future re-introduction of per-call headers as a regression.
      const { data } = await api.get<{ data: StatsRow[] }>(`/v1/analytics/stats?${params}`)
      stats.value = data.data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      statsError.value = getErrorMessage(e, "Failed to load statistics")
    } finally {
      statsLoading.value = false
    }
  }

  // ── Logs (raw call history) ──────────────────────────────────────
  // Plan section 1.2/1.3: filterable, paginated call log list.
  const logs = ref<CallLog[]>([])
  const logsLoading = ref(false)
  const logsError = ref<string | null>(null)
  const logsOffset = ref(0)
  const logsLimit = ref(DEFAULT_LOG_LIMIT)
  // Precise "has next page" indicator: we request `limit + 1` rows and
  // flip this when the server actually returns the extra sentinel row.
  // This avoids the "last page is exactly full → fake Next button → empty
  // result" footgun that comes from comparing `logs.length >= limit`.
  const hasMoreLogs = ref(false)

  /**
   * Push the current `logsOffset`/`logsLimit` into the URL when omitted from
   * the filter, so pagination state survives consecutive fetches. Caller
   * can override either by passing them explicitly (used by `resetLogsPage`).
   *
   * The server caps `limit` at `MAX_LOG_LIMIT = 500`, and we ask for
   * `limit + 1` purely to detect whether another page exists; the
   * sentinel row is trimmed before assigning to `logs`.
   */
  async function fetchLogs(filter: LogFilter = {}) {
    logsLoading.value = true
    logsError.value = null
    try {
      const effectiveLimit = filter.limit ?? logsLimit.value
      const params = new URLSearchParams()
      if (filter.provider) params.set("provider", filter.provider)
      if (filter.model) params.set("model", filter.model)
      if (filter.real_model) params.set("real_model", filter.real_model)
      if (filter.status_code != null) params.set("status_code", String(filter.status_code))
      params.set("offset", String(filter.offset ?? logsOffset.value))
      // Probe for an extra row to determine `hasMoreLogs` precisely.
      // TODO(plan-stage-1.x, see .plan/analytics-evolution.md §1.3
      // "前端：基础列表页"): if `effectiveLimit` is ever made
      // user-configurable up to `MAX_LOG_LIMIT = 500` (currently
      // hardcoded to 50), the +1 sentinel forces a 501 request that
      // the server clamps back to 500 — `rows.length` then never
      // exceeds `effectiveLimit`, so `hasMoreLogs` goes falsely
      // negative on a non-final page. Either cap the probe at
      // MAX_LOG_LIMIT (and accept that the very last page has no
      // visible sentinel) or expose a `Total` field from the server
      // and compare against it. Hardcoded default 50 never triggers;
      // safe to defer until §1.3 exposes page-size controls.
      params.set("limit", String(effectiveLimit + 1))
      // No per-call headers — Step 1 (A2) makes the global
      // `api.defaults.headers.common.Authorization` carry the Bearer
      // token for all `/api/*` calls from this store. See
      // `stores/connection.ts:saveConnection` and
      // `scripts/lint-api-auth.mjs`.
      const { data } = await api.get<{ data: CallLog[] }>(`/api/logs?${params}`)
      const rows = data.data
      hasMoreLogs.value = rows.length > effectiveLimit
      logs.value = hasMoreLogs.value ? rows.slice(0, effectiveLimit) : rows
      logsOffset.value = filter.offset ?? logsOffset.value
      logsLimit.value = effectiveLimit
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      logsError.value = getErrorMessage(e, "Failed to load call logs")
      // The actual offset mutation lives in nextLogsPage/prevLogsPage via
      // `filter.offset`, so a failed fetch leaves `logsOffset` untouched
      // and the user automatically stays on the page they were viewing.
      // No rollback math needed here — pre-vs-post offset is symmetric for
      // both Next and Prev, and explicit-offset callers (future use) won't
      // accidentally lose state to this catch.
    } finally {
      logsLoading.value = false
    }
  }

  /** Reset to page 0 (typically called after the user changes a filter). */
  function resetLogsPage() {
    logsOffset.value = 0
  }

  /**
   * Page navigation: compute the target offset and pass it explicitly via
   * `filter.offset` so `fetchLogs` only commits it on success. This avoids
   * the asymmetric-rollback bug that a naïve `+=` / `-=` of state before
   * the await would create — the catch path then doesn't need to know
   * whether the bump was `next` (+limit) or `prev` (-limit).
   */
  async function nextLogsPage() {
    await fetchLogs({ offset: logsOffset.value + logsLimit.value })
  }

  async function prevLogsPage() {
    await fetchLogs({ offset: Math.max(0, logsOffset.value - logsLimit.value) })
  }

  // ── Cross-tab summary (dashboard cards) ──────────────────────────
  const summary = computed(() => {
    const p = useProviderStore()
    const r = useRouteStore()
    return {
      providerCount: p.providers.length,
      routeCount: r.routes.length,
      totalCalls: stats.value.reduce((s, row) => s + row.call_count, 0),
      totalErrors: stats.value.reduce((s, row) => s + row.error_count, 0),
    }
  })

  return {
    // stats
    stats,
    statsLoading,
    statsError,
    fetchStats,
    summary,
    // logs
    logs,
    logsLoading,
    logsError,
    logsOffset,
    logsLimit,
    hasMoreLogs,
    fetchLogs,
    resetLogsPage,
    nextLogsPage,
    prevLogsPage,
  }
})

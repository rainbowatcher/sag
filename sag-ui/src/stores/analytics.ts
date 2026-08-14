import { defineStore } from "pinia"
import { ref, computed } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type {
  AnalyticsSummaryResponse,
  AnalyticsTrendResponse,
  AnalyticsV2Query,
  CacheStatsRow,
  CallLog,
  DistributionRow,
  LogFilter,
  LogsResponse,
  StatsRow,
  TrendRow,
} from "@/types/api"
import { useConnectionStore } from "./connection"
import { useProviderStore } from "./providers"
import { useRouteStore } from "./routes"

const DEFAULT_LOG_LIMIT = 50

/**
 * Monotonic generation guard for async queries. `next()` starts a new
 * generation and returns its token; `isCurrent(token)` is true only for the
 * latest generation. An async flow captures the token before awaiting and
 * re-checks it after the response/error lands — a stale result (token no
 * longer current) must not touch state. This is the store-level fix for the
 * out-of-order-response race: a slow response from a superseded scope/range
 * query can no longer overwrite newer data or errors.
 */
function createQueryGuard() {
  let current = 0
  return {
    /** Bump the generation and return the token of the new (current) one. */
    next() {
      current += 1
      return current
    },
    /** True iff `token` still belongs to the latest generation. */
    isCurrent(token: number) {
      return token === current
    },
  }
}

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
  const logs = ref<CallLog[]>([])
  const logsTotal = ref(0)
  const logsLoading = ref(false)
  const logsError = ref<string | null>(null)
  const logsOffset = ref(0)
  const logsLimit = ref(DEFAULT_LOG_LIMIT)
  const hasMoreLogs = ref(false)
  const activeLogFilter = ref<LogFilter>({})

  /**
   * Apply a new filter, reset offset to page 0, and fetch. Called by the
   * component when the user clicks the "Query" button.
   */
  async function applyLogsFilter(filter: LogFilter) {
    activeLogFilter.value = {
      provider: filter.provider,
      model: filter.model,
      real_model: filter.real_model,
      status_code: filter.status_code,
    }
    logsOffset.value = 0
    await fetchLogs()
  }

  /**
   * Fetch the current page of logs. The persisted `activeLogFilter` is
   * always merged in, and the caller can override specific fields (e.g.
   * `offset` for pagination). When called with no arguments the offset
   * falls back to `logsOffset.value`, so a bare `fetchLogs()` reloads the
   * current page in place — used by the "Refresh" button.
   */
  async function fetchLogs(overrides: LogFilter = {}) {
    logsLoading.value = true
    logsError.value = null
    try {
      const filter: LogFilter = { ...activeLogFilter.value, ...overrides }
      const effectiveLimit = filter.limit ?? logsLimit.value
      const targetOffset = filter.offset ?? logsOffset.value
      const params = new URLSearchParams()
      if (filter.provider) params.set("provider", filter.provider)
      if (filter.model) params.set("model", filter.model)
      if (filter.real_model) params.set("real_model", filter.real_model)
      if (filter.status_code != null) params.set("status_code", String(filter.status_code))
      params.set("offset", String(targetOffset))
      params.set("limit", String(effectiveLimit))
      const { data } = await api.get<LogsResponse>(`/api/logs?${params}`)
      logs.value = data.data ?? []
      logsTotal.value = data.total ?? 0
      hasMoreLogs.value = targetOffset + logs.value.length < logsTotal.value
      logsOffset.value = targetOffset
      logsLimit.value = effectiveLimit
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      logsError.value = getErrorMessage(e, "Failed to load call logs")
    } finally {
      logsLoading.value = false
    }
  }

  /** Direct 1-based page navigation. */
  async function setLogsPage(page: number) {
    const targetOffset = Math.max(0, (page - 1) * logsLimit.value)
    await fetchLogs({ offset: targetOffset })
  }

  /** Change page size and reset offset to page 1. */
  async function setLogsLimit(limit: number) {
    await fetchLogs({ offset: 0, limit })
  }

  /**
   * Page navigation: compute the target offset and pass it explicitly via
   * the `overrides` argument so `fetchLogs` only commits it on success.
   * This avoids the asymmetric-rollback bug that a naïve `+=` / `-=` of
   * state before the await would create — the catch path then doesn't
   * need to know whether the bump was `next` (+limit) or `prev` (-limit).
   */
  async function nextLogsPage() {
    await fetchLogs({ offset: logsOffset.value + logsLimit.value })
  }

  async function prevLogsPage() {
    await fetchLogs({ offset: Math.max(0, logsOffset.value - logsLimit.value) })
  }

  // ── Cache (hit-rate stats) ───────────────────────────────────────
  const cache = ref<CacheStatsRow[]>([])
  const cacheLoading = ref(false)
  const cacheError = ref<string | null>(null)

  async function fetchCacheStats(provider?: string, model?: string) {
    cacheLoading.value = true
    cacheError.value = null
    try {
      const params = new URLSearchParams()
      if (provider) params.set("provider", provider)
      if (model) params.set("model", model)
      const { data } = await api.get<{ data: CacheStatsRow[] }>(`/v1/analytics/cache?${params}`)
      cache.value = data.data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      cacheError.value = getErrorMessage(e, "Failed to load cache statistics")
    } finally {
      cacheLoading.value = false
    }
  }

  // ── Trend (time-bucketed) ───────────────────────────────────────
  const trend = ref<TrendRow[]>([])
  const trendLoading = ref(false)
  const trendError = ref<string | null>(null)

  async function fetchTrend(
    bucketSeconds: number,
    fromMs: number,
    toMs?: number,
    provider?: string,
  ) {
    trendLoading.value = true
    trendError.value = null
    try {
      const params = new URLSearchParams()
      params.set("bucket_seconds", String(bucketSeconds))
      params.set("from", String(fromMs))
      if (toMs != null) params.set("to", String(toMs))
      if (provider) params.set("provider", provider)
      const { data } = await api.get<{ data: TrendRow[] }>(`/v1/analytics/trend?${params}`)
      trend.value = data.data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      trendError.value = getErrorMessage(e, "Failed to load trend")
    } finally {
      trendLoading.value = false
    }
  }

  // ── Distribution (proportions) ───────────────────────────────────
  const distribution = ref<DistributionRow[]>([])
  const distLoading = ref(false)
  const distError = ref<string | null>(null)

  async function fetchDistribution(provider?: string, model?: string) {
    distLoading.value = true
    distError.value = null
    try {
      const params = new URLSearchParams()
      if (provider) params.set("provider", provider)
      if (model) params.set("model", model)
      const { data } = await api.get<{ data: DistributionRow[] }>(
        `/v1/analytics/distribution?${params}`,
      )
      distribution.value = data.data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      distError.value = getErrorMessage(e, "Failed to load distribution")
    } finally {
      distLoading.value = false
    }
  }

  // ── Cross-tab summary (dashboard cards) ──────────────────────────
  const summary = computed(() => {
    const p = useProviderStore()
    const r = useRouteStore()
    let totalCalls = 0
    let totalErrors = 0
    for (const row of stats.value) {
      totalCalls += row.call_count
      totalErrors += row.error_count
    }
    return {
      providerCount: p.providers.length,
      routeCount: r.routes.length,
      totalCalls,
      totalErrors,
    }
  })

  // ── V2 unified query API (summary / trend) ──────────────────────
  const v2Summary = ref<AnalyticsSummaryResponse | null>(null)
  const v2SummaryLoading = ref(false)
  const v2SummaryError = ref<string | null>(null)
  const v2Trend = ref<AnalyticsTrendResponse | null>(null)
  const v2TrendLoading = ref(false)
  const v2TrendError = ref<string | null>(null)
  /** Summary and trend have independent generations: a stale response of
   *  one kind never affects the other kind's state. */
  const v2SummaryGuard = createQueryGuard()
  const v2TrendGuard = createQueryGuard()

  /** Serialize the shared v2 query parameters (from/to are always set). */
  function buildV2Params(query: AnalyticsV2Query): URLSearchParams {
    const params = new URLSearchParams()
    params.set("from", String(query.from))
    params.set("to", String(query.to))
    if (query.scope) params.set("scope", query.scope)
    if (query.route) params.set("route", query.route)
    if (query.provider) params.set("provider", query.provider)
    if (query.real_model) params.set("real_model", query.real_model)
    if (query.top != null) params.set("top", String(query.top))
    return params
  }

  /**
   * Fetch the v2 summary (overall + groups + unassigned failures). Only the
   * request holding the current generation token may write summary state:
   * a late response/error from a superseded query is dropped, so rapid
   * scope/range/filter switches can never let stale results or stale errors
   * clobber the newest query's state. The current-token success path clears
   * the error when writing new data, so a surviving stale error can never
   * coexist with freshly written data (the error ref is owned by whichever
   * request is current at write time, not by who started the request).
   */
  async function fetchV2Summary(query: AnalyticsV2Query) {
    const token = v2SummaryGuard.next()
    v2SummaryLoading.value = true
    v2SummaryError.value = null
    try {
      const { data } = await api.get<AnalyticsSummaryResponse>(
        `/v1/analytics/v2/summary?${buildV2Params(query)}`,
      )
      if (!v2SummaryGuard.isCurrent(token)) return
      v2SummaryError.value = null
      v2Summary.value = data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      if (!v2SummaryGuard.isCurrent(token)) return
      v2SummaryError.value = getErrorMessage(e, "Failed to load analytics summary")
    } finally {
      if (v2SummaryGuard.isCurrent(token)) v2SummaryLoading.value = false
    }
  }

  /**
   * Fetch the v2 trend (per-hour buckets + Top-N series + others). Guarded
   * by its own generation token, independent of the summary's. Like the
   * summary, the current-token success path clears the error when writing.
   */
  async function fetchV2Trend(query: AnalyticsV2Query) {
    const token = v2TrendGuard.next()
    v2TrendLoading.value = true
    v2TrendError.value = null
    try {
      const { data } = await api.get<AnalyticsTrendResponse>(
        `/v1/analytics/v2/trend?${buildV2Params(query)}`,
      )
      if (!v2TrendGuard.isCurrent(token)) return
      v2TrendError.value = null
      v2Trend.value = data
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      if (!v2TrendGuard.isCurrent(token)) return
      v2TrendError.value = getErrorMessage(e, "Failed to load analytics trend")
    } finally {
      if (v2TrendGuard.isCurrent(token)) v2TrendLoading.value = false
    }
  }

  return {
    // stats
    stats,
    statsLoading,
    statsError,
    fetchStats,
    summary,
    // cache
    cache,
    cacheLoading,
    cacheError,
    fetchCacheStats,
    // trend / distribution
    trend,
    trendLoading,
    trendError,
    fetchTrend,
    distribution,
    distLoading,
    distError,
    fetchDistribution,
    // v2 unified query API
    v2Summary,
    v2SummaryLoading,
    v2SummaryError,
    fetchV2Summary,
    v2Trend,
    v2TrendLoading,
    v2TrendError,
    fetchV2Trend,
    // logs
    logs,
    logsTotal,
    logsLoading,
    logsError,
    logsOffset,
    logsLimit,
    hasMoreLogs,
    activeLogFilter,
    applyLogsFilter,
    fetchLogs,
    setLogsPage,
    setLogsLimit,
    nextLogsPage,
    prevLogsPage,
  }
})

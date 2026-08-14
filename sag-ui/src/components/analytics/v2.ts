import { formatDuration } from "@/lib/utils"
import type { AnalyticsGroupKey, AnalyticsScope, MetricSummary } from "@/types/api"

/** Human label of a group key: `route_name`, or `provider / real_model`. */
export function groupKeyLabel(key: AnalyticsGroupKey): string {
  if ("route_name" in key) return key.route_name
  return `${key.provider} / ${key.real_model}`
}

/**
 * Render a 0..1 fraction as a percentage; `null` / `NaN` → "—". Rate fields
 * are `null` when their denominator is 0 — never fabricated as 0.
 */
export function formatRate(v: number | null | undefined): string {
  if (v == null || Number.isNaN(v)) return "—"
  return `${(v * 100).toFixed(1)}%`
}

/** Render a count; `null` / `NaN` → "—". */
export function formatCount(v: number | null | undefined): string {
  return v == null || Number.isNaN(v) ? "—" : v.toLocaleString()
}

/** Render a per-second rate; `null` / `NaN` → "—". */
export function formatRatePerSec(v: number | null | undefined): string {
  return v == null || Number.isNaN(v) ? "—" : v.toFixed(2)
}

/** Render a latency value (ms) via the shared duration formatter. */
export function formatLatency(v: number | null | undefined): string {
  return v == null || Number.isNaN(v) ? "—" : formatDuration(v)
}

/** Population of a metric block for the given scope (requests vs attempts). */
export function populationOf(m: MetricSummary, scope: AnalyticsScope): number {
  return scope === "target" ? m.attempts : m.requests
}

/** Wording for the population metric of a scope. Target-scope population
 *  is shown as lowercase "requests" to distinguish it from route-scope
 *  "Requests". */
export function populationLabel(scope: AnalyticsScope): string {
  return scope === "target" ? "requests" : "Requests"
}

/** Per-second unit of the overall throughput metric — the backend's
 *  `request_throughput_per_sec` counts attempts in target scope, so the
 *  unit must not be hardcoded as `req/s`. */
export function throughputUnit(scope: AnalyticsScope): string {
  return scope === "target" ? "attempts/s" : "req/s"
}

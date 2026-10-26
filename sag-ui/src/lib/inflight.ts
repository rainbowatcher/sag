/**
 * Pure helpers behind the live in-flight counters (`GET /api/inflight` SSE).
 *
 * The backend pushes a **full** snapshot on connect and again on every change,
 * so the client state is replace-only (never merged, never accumulated). These
 * functions keep that logic out of the Pinia store so the node test runner can
 * cover it without a browser: parsing an `inflight` frame, folding a snapshot
 * into state, and the two connection transitions (open / drop).
 */
import type { InflightRouteCount, InflightSnapshot } from "@/types/api"

/** Reactive shape mirrored by the store. */
export interface InflightState {
  /** Last full snapshot received; `null` before the first frame. */
  snapshot: InflightSnapshot | null
  /** Whether the SSE stream is currently open. */
  connected: boolean
  /** The stream dropped while a snapshot was held: the counters may be outdated. */
  stale: boolean
}

/** State before the first frame: no data, not connected, nothing stale. */
export function emptyInflightState(): InflightState {
  return { snapshot: null, connected: false, stale: false }
}

/** Non-negative integer from a JSON value, or `null` when unusable. */
function toCount(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return null
  return Math.floor(value)
}

/**
 * Parse one `event: inflight` frame payload.
 *
 * Unknown fields are ignored (gateway design principle) and malformed route
 * entries are dropped instead of failing the whole snapshot, but a payload that
 * is not an object or has no usable `seq` is rejected — a garbage frame must
 * never blank the panel. Routes are re-sorted by count descending so the badge
 * order does not depend on the backend's sort being stable.
 */
export function parseInflightSnapshot(data: string): InflightSnapshot | null {
  let raw: unknown
  try {
    raw = JSON.parse(data)
  } catch {
    return null
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return null

  const record = raw as Record<string, unknown>
  const seq = toCount(record.seq)
  if (seq === null) return null

  const routes: InflightRouteCount[] = []
  if (Array.isArray(record.routes)) {
    for (const entry of record.routes) {
      if (typeof entry !== "object" || entry === null) continue
      const item = entry as Record<string, unknown>
      const name = typeof item.model_name === "string" ? item.model_name.trim() : ""
      const count = toCount(item.count)
      if (name === "" || count === null) continue
      routes.push({ model_name: name, count })
    }
  }
  routes.sort((a, b) => b.count - a.count)

  const total = toCount(record.total) ?? routes.reduce((sum, route) => sum + route.count, 0)
  return { seq, total, routes }
}

/**
 * Fold a snapshot into state: whole-object replacement plus "we are live".
 *
 * `stale` clears here (and only here) because a fresh snapshot is the only
 * proof that the displayed counters match the gateway again.
 */
export function applySnapshot(state: InflightState, snapshot: InflightSnapshot): InflightState {
  return { snapshot, connected: true, stale: false }
}

/**
 * Stream opened but no frame yet: mark the transport as live while keeping any
 * previous snapshot — and its `stale` flag, since nothing has been confirmed.
 */
export function markConnected(state: InflightState): InflightState {
  return { ...state, connected: true }
}

/**
 * Stream dropped: the last snapshot is kept (never zeroed — a reconnecting
 * panel showing 0 in-flight requests would lie about live traffic) and flagged
 * stale so the UI can dim it behind a "reconnecting" label.
 */
export function markDisconnected(state: InflightState): InflightState {
  return { ...state, connected: false, stale: state.snapshot !== null }
}

/** Badges are rendered for routes with traffic only, in descending count order. */
export function activeRouteCounts(routes: InflightRouteCount[]): InflightRouteCount[] {
  return routes.filter((route) => route.count > 0)
}

/**
 * Absolute stream URL from the axios `baseURL` the app already uses: an empty
 * baseURL means "same origin" (embedded build), an absolute one is a
 * separately served gateway.
 */
export function resolveSseUrl(baseURL: string | undefined | null, path: string): string {
  const base = (baseURL ?? "").replace(/\/+$/, "")
  return `${base}${path}`
}

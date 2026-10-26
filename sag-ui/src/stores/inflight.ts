import { defineStore } from "pinia"
import { computed, ref } from "vue"
import { api } from "@/lib/client"
import {
  activeRouteCounts,
  applySnapshot,
  emptyInflightState,
  markConnected,
  markDisconnected,
  parseInflightSnapshot,
  resolveSseUrl,
} from "@/lib/inflight"
import type { InflightState } from "@/lib/inflight"
import { createSseClient, headersFromDefaults, isAuthFailure } from "@/lib/sse"
import type { SseClient } from "@/lib/sse"
import type { InflightRouteCount, InflightSnapshot } from "@/types/api"
import { useConnectionStore } from "./connection"

/** Stream path of the live in-flight counters. */
const INFLIGHT_PATH = "/api/inflight"

/**
 * Live in-flight request counters, mirrored from `GET /api/inflight` (SSE).
 *
 * The backend re-sends a **full** snapshot on connect and on every change, so
 * this store only replaces its state — it never merges or accumulates. When the
 * stream drops the last snapshot is kept and flagged `stale` (never zeroed): a
 * reconnecting panel must not claim that nothing is in flight.
 */
export const useInflightStore = defineStore("inflight", () => {
  const snapshot = ref<InflightSnapshot | null>(null)
  const connected = ref(false)
  const stale = ref(false)

  // The SSE client lives outside the reactive state: it is a resource, not
  // data, and must not be proxied by Vue.
  let client: SseClient | null = null

  /** Snapshot plus transport flags, in the shape the pure helpers expect. */
  function currentState(): InflightState {
    return { snapshot: snapshot.value, connected: connected.value, stale: stale.value }
  }

  function assign(state: InflightState): void {
    snapshot.value = state.snapshot
    connected.value = state.connected
    stale.value = state.stale
  }

  /** Routes with traffic only, descending by count — the badge row's order. */
  const activeRoutes = computed<InflightRouteCount[]>(() =>
    activeRouteCounts(snapshot.value?.routes ?? []),
  )

  /** Total in-flight requests from the last snapshot (0 before the first frame). */
  const total = computed(() => snapshot.value?.total ?? 0)

  /** Whether there is live traffic to show (drives the "idle" placeholder). */
  const hasTraffic = computed(() => activeRoutes.value.length > 0)

  /** Feed one `inflight` frame payload; a malformed frame is ignored. */
  function onFrame(data: string): void {
    const parsed = parseInflightSnapshot(data)
    if (parsed === null) return
    assign(applySnapshot(currentState(), parsed))
  }

  /**
   * Headers for the SSE request: the same ones the shared axios instance puts on
   * every `/api/*` call, plus `x-api-key` for gateways that accept it on the
   * stream endpoint. `Authorization` stays authoritative for `/api/*`.
   */
  function requestHeaders(): Record<string, string> {
    const headers = headersFromDefaults(api.defaults.headers.common as Record<string, unknown>)
    const apiKey = useConnectionStore().connection?.apiKey
    if (apiKey) headers["x-api-key"] = apiKey
    return headers
  }

  /** Open the stream (no-op when already running). */
  function start(): void {
    if (client?.isRunning()) return
    client = createSseClient({
      url: resolveSseUrl(api.defaults.baseURL, INFLIGHT_PATH),
      headers: requestHeaders(),
      onOpen: () => assign(markConnected(currentState())),
      onEvent: (event) => {
        if (event.event !== "inflight") return
        onFrame(event.data)
      },
      onError: (error) => {
        if (isAuthFailure(error)) {
          // The key was rejected (rotated or revoked): hand it to the shared 401
          // handler, which clears the connection and redirects to /connect, and
          // stop here — retrying a 401 forever only hides the problem.
          client?.stop()
          useConnectionStore().handle401()
          return
        }
        assign(markDisconnected(currentState()))
      },
    })
    client.start()
  }

  /** Close the stream; the snapshot is kept so a re-open repaints instantly. */
  function stop(): void {
    client?.stop()
    client = null
    assign(markDisconnected(currentState()))
  }

  /**
   * Close the stream and clear the snapshot.
   *
   * Called by the connection store on disconnect / 401 (`handle401`) so the
   * previous gateway's or account's counters never linger as `stale` for a
   * round-trip after the connection is gone. A plain `stop()` deliberately keeps
   * the snapshot: a remount repaints instantly.
   */
  function reset(): void {
    stop()
    assign(emptyInflightState())
  }

  return {
    snapshot,
    connected,
    stale,
    activeRoutes,
    total,
    hasTraffic,
    start,
    stop,
    reset,
  }
})

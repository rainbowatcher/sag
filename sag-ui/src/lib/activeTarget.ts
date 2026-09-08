import type { ActiveTargetEntry, Strategy } from "@/types/api"

/**
 * Best sticky-failover active entry for a route: prefer a live `pinned` pin
 * over an `expired` one, then the most recently recorded (smallest
 * `age_secs`). Wildcard routes (`model_name: "*"`) can pin several targets
 * at once (one per request model), so a plain `find` would return an
 * arbitrary first match — this picks the most meaningful one.
 *
 * `strategy` is the route's *current* strategy (from the route config, not
 * from any entry's `strategy` field). Only `sticky_failover` routes have a
 * meaningful pin: `fill_first` and `round_robin` never write
 * `active_targets`, so a leftover pin on such a route is stale — it does not
 * reflect where traffic currently goes and must never be treated as the
 * active target (e.g. after a route switches from sticky_failover to
 * fill_first, the stale pin must not override the fill-first display).
 * Filtering by route strategy — not entry strategy — keeps this correct both
 * immediately after a strategy switch (entries still carry the old DTO) and
 * after a refresh (entries carry the new one).
 */
export function selectActiveTarget(
  entries: ActiveTargetEntry[],
  model: string,
  strategy: Strategy,
): ActiveTargetEntry | undefined {
  if (strategy !== "sticky_failover") return undefined
  const candidates = entries.filter((s) => s.model === model && s.status !== "idle")
  if (candidates.length === 0) return undefined
  const pinned = candidates.filter((s) => s.status === "pinned")
  const pool = pinned.length > 0 ? pinned : candidates
  return [...pool].sort((a, b) => a.age_secs - b.age_secs)[0]
}

/**
 * Current fill-first target for a route: the target at position 0 of the
 * cached shuffled order — the one currently absorbing all fill-first
 * traffic. Only meaningful for `fill_first` strategy routes (their entries
 * never carry a sticky pin, so `selectActiveTarget` is always undefined for
 * them); returns undefined when the route is not fill-first or no order is
 * cached.
 */
export function selectFillFirstTarget(
  entries: ActiveTargetEntry[],
  model: string,
): ActiveTargetEntry | undefined {
  return entries.find(
    (s) => s.model === model && s.strategy === "fill_first" && s.fill_first_position === 0,
  )
}

/**
 * Whether a sticky-failover `active` entry (pinned) corresponds to the
 * route's first target in its original (config) order — the green dot case.
 *
 * The `/api/active-targets` DTO carries each target's 0-based `target_index`
 * within `route.targets`, so the pin's config position is known directly:
 * index 0 is the first target. A missing or non-zero `target_index` (older
 * backend, malformed payload) returns `false` — the conservative choice: an
 * unverifiable pin renders orange, never green.
 */
export function isActiveFirstTarget(active: ActiveTargetEntry): boolean {
  return active.target_index === 0
}

/**
 * The model key to send to `DELETE /api/active-targets` for a displayed
 * active entry. Fill-first order caches live under the route model name;
 * sticky pins under the request-model key (`entry.key`, the concrete request
 * model the pin was recorded under).
 *
 * Defensive fallback: an entry whose `key` is empty (older backend shape,
 * malformed payload) falls back to the route model name — for a wildcard
 * route that is `"*"`, which the backend resolves to "clear all concrete
 * pins of the wildcard route", so the clear still works even when the
 * concrete key is unknown. Normal (non-empty-key) semantics are unchanged.
 */
export function clearActiveTargetModel(entry: ActiveTargetEntry, isFillFirst: boolean): string {
  return isFillFirst ? entry.model : entry.key || entry.model
}

/**
 * Status-dot CSS class for a route's active target, mirroring the cell in
 * RouteList.vue: fill-first traffic is always emerald; an expired sticky pin
 * is muted — gray-green when it hit the route's first config target,
 * gray-orange otherwise (muted hue = expired, color family = config
 * position); a live pin on the route's first config target is green and any
 * fallback is orange. Returns `null` when there is no active target to
 * display.
 */
export function activeTargetDotColor(
  sticky: ActiveTargetEntry | undefined,
  fill: ActiveTargetEntry | undefined,
): string | null {
  const active = sticky ?? fill
  if (!active) return null
  if (fill !== undefined && sticky === undefined) return "bg-emerald-500"
  const isFirst = isActiveFirstTarget(active)
  if (active.status === "expired") return isFirst ? "bg-green-500/40" : "bg-orange-500/40"
  return isFirst ? "bg-green-500" : "bg-orange-500"
}

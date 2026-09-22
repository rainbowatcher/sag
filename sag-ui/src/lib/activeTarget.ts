import type { ActiveTargetEntry } from "@/types/api"

/**
 * Best sticky-failover active entry for a route: prefer a live `pinned` pin
 * over an `expired` one, then the most recently recorded (smallest
 * `age_secs`). Wildcard routes (`model_name: "*"`) can pin several targets
 * at once (one per request model), so a plain `find` would return an
 * arbitrary first match — this picks the most meaningful one.
 *
 * Only `sticky_failover` entries carry a pin (`entry.strategy` filters);
 * `fill_first` / `round_robin` entries are never returned.
 */
export function selectActiveTarget(
  entries: ActiveTargetEntry[],
  model: string,
): ActiveTargetEntry | undefined {
  const candidates = entries.filter(
    (s) => s.model === model && s.strategy === "sticky_failover" && s.status !== "idle",
  )
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
 * route's highest-priority (primary) target — the green dot case.
 *
 * The `/api/active-targets` DTO carries each target's 0-based `target_index`
 * in the route's priority-sorted order (priority descending, ties keep array
 * order): index 0 is the highest-priority target.
 */
export function isActiveFirstTarget(active: ActiveTargetEntry): boolean {
  return active.target_index === 0
}

/**
 * The model key to send to `DELETE /api/active-targets` for a displayed
 * active entry. Fill-first order caches live under the route model name;
 * sticky pins under the request-model key (`entry.key`, the concrete request
 * model the pin was recorded under).
 */
export function clearActiveTargetModel(entry: ActiveTargetEntry, isFillFirst: boolean): string {
  return isFillFirst ? entry.model : entry.key
}

/**
 * Status-dot CSS class for a route's active target, mirroring the cell in
 * RouteList.vue: fill-first traffic is always emerald; an expired sticky pin
 * is muted — gray-green when it hit the route's highest-priority target,
 * gray-orange otherwise (muted hue = expired, color family = priority
 * rank); a live pin on the route's highest-priority target is green and any
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

import type { ActiveTargetEntry, RouteConfig } from "@/types/api"

/**
 * Best sticky-failover active entry for a route: prefer a live `pinned` pin
 * over an `expired` one, then the most recently recorded (smallest
 * `age_secs`). Wildcard routes (`model_name: "*"`) can pin several targets
 * at once (one per request model), so a plain `find` would return an
 * arbitrary first match — this picks the most meaningful one.
 */
export function selectActiveTarget(
  entries: ActiveTargetEntry[],
  model: string,
): ActiveTargetEntry | undefined {
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
 * The `/api/active-targets` DTO does not carry a target index, but the
 * backend emits exactly one entry per config target, in config order, and
 * reports each pin on the DTO of the target it was pinned through (provider +
 * real-model-spec matching — a passthrough pin is never attributed to an
 * explicit target or vice versa). The store preserves that response order and
 * `model` is the unique route `model_name`, so the first `entries` item for
 * this route is the first config target's DTO — and the active pin is the
 * first target iff it is that exact object. Comparing object identity is more
 * precise than comparing the resolved `real_model`: it cannot confuse a
 * passthrough pin with a later explicit target that shares the same resolved
 * model (e.g. an exact route whose passthrough resolves to the route name).
 *
 * Returns `false` for empty target lists and whenever the DTO cannot confirm
 * the pin landed on the first target — the conservative choice: an
 * unverifiable pin renders orange, never green.
 */
export function isActiveFirstTarget(
  route: RouteConfig,
  active: ActiveTargetEntry,
  entries: ActiveTargetEntry[],
): boolean {
  const first = route.targets[0]
  if (!first) return false
  // Defensive: the first target's DTO always carries its provider, so this
  // is redundant with the identity check below but an explicit cheap guard.
  if (active.provider !== first.provider) return false
  const routeEntry = entries.find((e) => e.model === route.model_name)
  return routeEntry !== undefined && active === routeEntry
}

/**
 * Status-dot CSS class for a route's active target, mirroring the cell in
 * RouteList.vue: fill-first traffic is always emerald; an expired sticky pin
 * is always gray (taking priority over green/orange); otherwise a pin on the
 * route's first config target is green and any fallback is orange. Returns
 * `null` when there is no active target to display.
 */
export function activeTargetDotColor(
  route: RouteConfig,
  sticky: ActiveTargetEntry | undefined,
  fill: ActiveTargetEntry | undefined,
  entries: ActiveTargetEntry[],
): string | null {
  const active = sticky ?? fill
  if (!active) return null
  if (fill !== undefined && sticky === undefined) return "bg-emerald-500"
  if (active.status === "expired") return "bg-gray-500"
  return isActiveFirstTarget(route, active, entries) ? "bg-green-500" : "bg-orange-500"
}

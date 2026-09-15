import assert from "node:assert/strict"
import type { ActiveTargetEntry, RouteConfig, Strategy } from "../types/api.ts"
import {
  activeTargetDotColor,
  clearActiveTargetModel,
  isActiveFirstTarget,
  selectActiveTarget,
  selectFillFirstTarget,
} from "./activeTarget.ts"

// Boundary tests for the active-target "primary target" / status-dot logic.
// The backend `/api/active-targets` emits one DTO per config target, in
// config order, each carrying its 0-based `target_index` — the target's rank
// in the route's priority-sorted order (priority descending, ties keep array
// order), the source of truth for "did the pin land on the route's
// highest-priority (primary) target". These tests exercise the index-based
// discrimination against those shapes.

function target(provider: string, realModel: string | null): RouteConfig["targets"][number] {
  return { provider, real_model: realModel, priority: 0 }
}

function route(
  modelName: string,
  targets: RouteConfig["targets"],
  strategy: Strategy = "sticky_failover",
): RouteConfig {
  return {
    model_name: modelName,
    api_endpoint: "/chat/completions",
    strategy,
    targets,
  }
}

function entry(
  partial: Partial<ActiveTargetEntry> & { model: string; target_index: number },
): ActiveTargetEntry {
  return {
    key: "",
    provider: "openai",
    real_model: "",
    age_secs: 0,
    status: "idle",
    strategy: "sticky_failover",
    fill_first_position: null,
    fill_first_cache_age_secs: null,
    fill_first_cache_ttl_secs: null,
    ...partial,
  }
}

// ── target_index: first (0) vs non-first (1+) ──────────────────────
{
  const first = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(isActiveFirstTarget(first), true, "target_index 0 is the highest-priority target")
  assert.equal(activeTargetDotColor(first, undefined), "bg-green-500")

  const fallback = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    target_index: 1,
    status: "pinned",
  })
  assert.equal(isActiveFirstTarget(fallback), false, "target_index 1 is a fallback")
  assert.equal(activeTargetDotColor(fallback, undefined), "bg-orange-500")

  // A lower-priority rank is equally non-primary.
  assert.equal(
    isActiveFirstTarget(entry({ model: "gpt-4o", target_index: 2, status: "pinned" })),
    false,
  )
}

// ── missing / invalid target_index → conservative false (orange) ──
{
  // Older backend that has not shipped `target_index` yet: the field is
  // absent at runtime, `undefined === 0` is false → never green.
  const legacy = entry({ model: "gpt-4o", target_index: 0, status: "pinned" })
  delete (legacy as { target_index?: number }).target_index
  assert.equal(isActiveFirstTarget(legacy), false, "missing target_index is never first")
  assert.equal(activeTargetDotColor(legacy, undefined), "bg-orange-500")

  // Malformed / out-of-range values are also conservatively non-first.
  const negative = entry({ model: "gpt-4o", target_index: -1, status: "pinned" })
  assert.equal(isActiveFirstTarget(negative), false)
  assert.equal(activeTargetDotColor(negative, undefined), "bg-orange-500")

  const nan = entry({ model: "gpt-4o", target_index: Number.NaN, status: "pinned" })
  assert.equal(isActiveFirstTarget(nan), false)

  const fractional = entry({ model: "gpt-4o", target_index: 0.5, status: "pinned" })
  assert.equal(isActiveFirstTarget(fractional), false)
}

// ── expired mutes the config-position hue (gray-green / gray-orange) ──
{
  const expiredFirst = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "expired",
    age_secs: 9999,
  })
  assert.equal(
    isActiveFirstTarget(expiredFirst),
    true,
    "index is independent of expiry (expired pins keep their priority rank)",
  )
  assert.equal(activeTargetDotColor(expiredFirst, undefined), "bg-green-500/40")

  const expiredFallback = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    target_index: 1,
    status: "expired",
    age_secs: 9999,
  })
  assert.equal(isActiveFirstTarget(expiredFallback), false)
  assert.equal(activeTargetDotColor(expiredFallback, undefined), "bg-orange-500/40")

  // Expired pin with a missing index is equally unverifiable → gray-orange.
  const expiredLegacy = entry({
    model: "gpt-4o",
    target_index: 0,
    status: "expired",
    age_secs: 9999,
  })
  delete (expiredLegacy as { target_index?: number }).target_index
  assert.equal(activeTargetDotColor(expiredLegacy, undefined), "bg-orange-500/40")
}

// ── fill-first stays emerald and is not affected by sticky logic ─────
{
  const fill = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: 0,
    status: "idle",
  })
  assert.equal(selectFillFirstTarget([fill], "gpt-4o"), fill)
  assert.equal(activeTargetDotColor(undefined, fill), "bg-emerald-500")

  // If a sticky pin coexists, fill-first does not override it (the cell
  // prefers sticky) — the color then follows the sticky entry.
  const sticky = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(activeTargetDotColor(sticky, fill), "bg-green-500")

  // Ambiguous edge: a stale pin whose target also sits at position 0 of the
  // cached order. The cell displays it as the fill target (emerald) — the
  // fill-first view wins because selectActiveTarget never returns a pin for
  // a fill_first route.
  const ambiguous = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: 0,
    status: "pinned",
  })
  assert.equal(selectActiveTarget([ambiguous], "gpt-4o", "fill_first"), undefined)
  assert.equal(selectFillFirstTarget([ambiguous], "gpt-4o"), ambiguous)
  assert.equal(activeTargetDotColor(undefined, ambiguous), "bg-emerald-500")
}

// ── fill_first with no cached order is NOT an active target ─────────
{
  // A fill_first route that has never been exercised has no cached order:
  // every entry reports `fill_first_position: null`. The table's "Active
  // Target" sort must not count such a route as active — only position 0
  // of a cached order is the current fill target.
  const noOrder = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: null,
    status: "idle",
  })
  assert.equal(
    selectFillFirstTarget([noOrder], "gpt-4o"),
    undefined,
    "fill_first without a cached order has no active target",
  )
  assert.equal(activeTargetDotColor(undefined, undefined), null, "no active → no dot")
}

// ── selectActiveTarget: only sticky_failover routes use pins ────────
{
  // A leftover pin on a route that switched to fill_first must never be
  // treated as the active target — neither pre-refresh (old DTO still says
  // "sticky_failover") nor post-refresh (DTO says "fill_first").
  const stalePreRefresh = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
    strategy: "sticky_failover",
  })
  const stalePostRefresh = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
    strategy: "fill_first",
  })
  assert.equal(selectActiveTarget([stalePreRefresh], "gpt-4o", "fill_first"), undefined)
  assert.equal(selectActiveTarget([stalePostRefresh], "gpt-4o", "fill_first"), undefined)
  assert.equal(
    selectActiveTarget([stalePostRefresh], "gpt-4o", "round_robin"),
    undefined,
    "round_robin routes also never display stale pins",
  )

  // The same entry on a sticky_failover route IS the active target.
  const r = route("gpt-4o", [target("openai", "gpt-4o")])
  const pin = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(selectActiveTarget([pin], "gpt-4o", r.strategy), pin)

  // Cell-level equivalence: a fill_first route with a stale pin and a live
  // fill target shows the fill target (emerald), never the pin.
  const fill = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: 0,
    status: "idle",
  })
  const sticky = selectActiveTarget([stalePostRefresh, fill], "gpt-4o", "fill_first")
  const fillActive = selectFillFirstTarget([stalePostRefresh, fill], "gpt-4o")
  assert.equal(sticky, undefined)
  assert.equal(fillActive, fill)
  assert.equal(activeTargetDotColor(sticky, fillActive), "bg-emerald-500")
}

// ── selection semantics preserved: pinned preferred, newest wins ────
{
  const older = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
    age_secs: 100,
  })
  const newer = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4.1",
    target_index: 1,
    status: "pinned",
    age_secs: 5,
  })
  assert.equal(
    selectActiveTarget([older, newer], "gpt-4o", "sticky_failover"),
    newer,
    "most recently recorded pin is the active entry",
  )
  assert.equal(
    selectActiveTarget([newer, older], "gpt-4o", "sticky_failover"),
    newer,
    "age, not array order, picks the active entry",
  )

  // A live pinned entry beats an expired one regardless of age.
  const expired = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    target_index: 1,
    status: "expired",
    age_secs: 1,
  })
  assert.equal(
    selectActiveTarget([expired, older], "gpt-4o", "sticky_failover"),
    older,
    "pinned wins over expired",
  )
}

// ── model-scoped lookup: one route's pin never leaks into another ───
{
  const aPinned = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    target_index: 1,
    status: "pinned",
  })
  const bPinned = entry({
    model: "gpt-4o-vision",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  const entries = [bPinned, aPinned]

  // Route A: pin on its second config target → orange.
  assert.equal(selectActiveTarget(entries, "gpt-4o", "sticky_failover"), aPinned)
  assert.equal(isActiveFirstTarget(aPinned), false)
  assert.equal(activeTargetDotColor(aPinned, undefined), "bg-orange-500")

  // Route B: pin on its only (first) target → green, and A's pin never
  // leaks in.
  assert.equal(selectActiveTarget(entries, "gpt-4o-vision", "sticky_failover"), bPinned)
  assert.equal(isActiveFirstTarget(bPinned), true)
  assert.equal(activeTargetDotColor(bPinned, undefined), "bg-green-500")

  // A route with no pin at all gets no dot, even with another's pin nearby.
  assert.equal(selectActiveTarget(entries, "gpt-4o-turbo", "sticky_failover"), undefined)
  assert.equal(activeTargetDotColor(undefined, undefined), null)
}

// ── no active / empty entries → no dot ─────────────────────────────
{
  assert.equal(activeTargetDotColor(undefined, undefined), null, "no active → no dot")
  assert.equal(selectActiveTarget([], "gpt-4o", "sticky_failover"), undefined)
  assert.equal(selectFillFirstTarget([], "gpt-4o"), undefined)
}

// ── clearActiveTargetModel: key || model defensive fallback ──────────
{
  // Sticky pin on an exact route: key equals the route model name.
  const exact = entry({
    model: "gpt-4o",
    key: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(clearActiveTargetModel(exact, false), "gpt-4o")

  // Sticky pin on a wildcard route: the concrete request-model key is sent
  // so only that one pin is cleared.
  const wildcard = entry({
    model: "*",
    key: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(clearActiveTargetModel(wildcard, false), "gpt-4o")

  // Defensive fallback: an empty key (older backend / malformed payload)
  // falls back to the route model name — for a wildcard route that is "*",
  // which the backend resolves to clearing all of the route's concrete pins.
  const emptyKey = entry({
    model: "*",
    key: "",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(clearActiveTargetModel(emptyKey, false), "*", "empty key falls back to model")
  assert.equal(
    clearActiveTargetModel({ ...emptyKey, model: "gpt-4o" }, false),
    "gpt-4o",
    "empty key on an exact route falls back to its model name",
  )

  // Fill-first clear always uses the route model name (order cache key).
  const fill = entry({
    model: "gpt-4o",
    strategy: "fill_first",
    fill_first_position: 0,
    target_index: 0,
    status: "idle",
  })
  assert.equal(clearActiveTargetModel(fill, true), "gpt-4o")
}

console.log("activeTarget.test: all assertions passed")

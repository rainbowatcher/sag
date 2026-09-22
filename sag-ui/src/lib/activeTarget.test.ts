import assert from "node:assert/strict"
import type { ActiveTargetEntry } from "../types/api.ts"
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

  // Synthetic input for activeTargetDotColor priority: a sticky pin and a
  // fill-first entry on the same model coexist. Real GET responses never
  // mix strategies for one model — this is not a cell-realistic scenario,
  // only a color-precedence check (sticky wins over fill).
  const sticky = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(activeTargetDotColor(sticky, fill), "bg-green-500")

  // Ambiguous edge: a pin whose target also sits at position 0 of the
  // cached order on a fill_first route. The cell displays it as the fill
  // target (emerald) — the fill-first view wins because selectActiveTarget
  // never returns a fill_first entry.
  const ambiguous = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: 0,
    status: "pinned",
  })
  assert.equal(selectActiveTarget([ambiguous], "gpt-4o"), undefined)
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

// ── selectActiveTarget: only sticky_failover entries are pins ───────
{
  // A sticky pin on a sticky_failover entry is the active target.
  const pin = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(selectActiveTarget([pin], "gpt-4o"), pin)

  // Cell-level equivalence: a fill_first route shows the fill target
  // (emerald), never a sticky entry.
  const fill = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "fill_first",
    fill_first_position: 0,
    status: "idle",
  })
  const sticky = selectActiveTarget([fill], "gpt-4o")
  const fillActive = selectFillFirstTarget([fill], "gpt-4o")
  assert.equal(sticky, undefined)
  assert.equal(fillActive, fill)
  assert.equal(activeTargetDotColor(sticky, fillActive), "bg-emerald-500")

  // round_robin is likewise not a pin: selectActiveTarget ignores it.
  const roundRobin = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    strategy: "round_robin",
    status: "pinned",
  })
  assert.equal(selectActiveTarget([roundRobin], "gpt-4o"), undefined)
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
    selectActiveTarget([older, newer], "gpt-4o"),
    newer,
    "most recently recorded pin is the active entry",
  )
  assert.equal(
    selectActiveTarget([newer, older], "gpt-4o"),
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
  assert.equal(selectActiveTarget([expired, older], "gpt-4o"), older, "pinned wins over expired")
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
  assert.equal(selectActiveTarget(entries, "gpt-4o"), aPinned)
  assert.equal(isActiveFirstTarget(aPinned), false)
  assert.equal(activeTargetDotColor(aPinned, undefined), "bg-orange-500")

  // Route B: pin on its only (first) target → green, and A's pin never
  // leaks in.
  assert.equal(selectActiveTarget(entries, "gpt-4o-vision"), bPinned)
  assert.equal(isActiveFirstTarget(bPinned), true)
  assert.equal(activeTargetDotColor(bPinned, undefined), "bg-green-500")

  // A route with no pin at all gets no dot, even with another's pin nearby.
  assert.equal(selectActiveTarget(entries, "gpt-4o-turbo"), undefined)
  assert.equal(activeTargetDotColor(undefined, undefined), null)
}

// ── no active / empty entries → no dot ─────────────────────────────
{
  assert.equal(activeTargetDotColor(undefined, undefined), null, "no active → no dot")
  assert.equal(selectActiveTarget([], "gpt-4o"), undefined)
  assert.equal(selectFillFirstTarget([], "gpt-4o"), undefined)
}

// ── clearActiveTargetModel: sticky uses entry.key, fill uses model ──
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

  // Sticky pin on a wildcard route: clears entry.key (the concrete request
  // model), not "*".
  const wildcard = entry({
    model: "*",
    key: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    target_index: 0,
    status: "pinned",
  })
  assert.equal(clearActiveTargetModel(wildcard, false), "gpt-4o")

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

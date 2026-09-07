import assert from "node:assert/strict"
import type { ActiveTargetEntry, RouteConfig } from "../types/api.ts"
import {
  activeTargetDotColor,
  isActiveFirstTarget,
  selectActiveTarget,
  selectFillFirstTarget,
} from "./activeTarget.ts"

// Boundary tests for the active-target "first target" / status-dot logic.
// The backend `/api/active-targets` emits one DTO per config target, in
// config order, and reports each pin on the DTO of the target it was pinned
// through; the store preserves that response order. These tests exercise the
// identity-based discrimination against those shapes.

function target(provider: string, realModel: string | null): RouteConfig["targets"][number] {
  return { provider, real_model: realModel, priority: 0 }
}

function route(modelName: string, targets: RouteConfig["targets"]): RouteConfig {
  return {
    model_name: modelName,
    api_endpoint: "/chat/completions",
    strategy: "sticky_failover",
    targets,
  }
}

function entry(partial: Partial<ActiveTargetEntry> & { model: string }): ActiveTargetEntry {
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

// ── exact route: passthrough first target + explicit fallback with the
//    same resolved model ───────────────────────────────────────────────
{
  const r = route("gpt-4o", [target("openai", null), target("openai", "gpt-4o")])
  // First (passthrough) target idle, explicit fallback pinned — both DTOs
  // resolve to real_model "gpt-4o", so field comparison cannot tell them
  // apart. The fallback must NOT be treated as the first target.
  const firstDto = entry({ model: "gpt-4o", provider: "openai", real_model: "gpt-4o" })
  const fallbackDto = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  const entries = [firstDto, fallbackDto]
  const active = selectActiveTarget(entries, "gpt-4o")
  assert.equal(active, fallbackDto, "the pinned fallback is the active entry")
  assert.equal(
    isActiveFirstTarget(r, active, entries),
    false,
    "explicit fallback with same resolved model must not be green",
  )
  assert.equal(activeTargetDotColor(r, active, undefined, entries), "bg-orange-500")

  // Same route, but the passthrough (first) target is the pinned one → green.
  const pinnedFirst = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  const entries2 = [
    pinnedFirst,
    entry({ model: "gpt-4o", provider: "openai", real_model: "gpt-4o" }),
  ]
  const active2 = selectActiveTarget(entries2, "gpt-4o")
  assert.equal(active2, pinnedFirst)
  assert.equal(
    isActiveFirstTarget(r, active2, entries2),
    true,
    "passthrough first target pinned → green",
  )
  assert.equal(activeTargetDotColor(r, active2, undefined, entries2), "bg-green-500")
}

// ── wildcard route: passthrough first target + explicit fallback on the
//    same provider ─────────────────────────────────────────────────────
{
  const r = route("*", [target("openai", null), target("openai", "gpt-4o")])
  // The explicit target is pinned via some request model; the passthrough
  // first target is idle (real_model empty). Provider identity alone would
  // wrongly mark this green — it must be orange.
  const firstDto = entry({ model: "*", provider: "openai" })
  const fallbackDto = entry({
    model: "*",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  const entries = [firstDto, fallbackDto]
  const active = selectActiveTarget(entries, "*")
  assert.equal(active, fallbackDto)
  assert.equal(
    isActiveFirstTarget(r, active, entries),
    false,
    "wildcard explicit fallback on the same provider must not be green",
  )
  assert.equal(activeTargetDotColor(r, active, undefined, entries), "bg-orange-500")

  // Wildcard passthrough first target pinned by a concrete request model →
  // green: the pin landed on the first target's DTO.
  const pinnedFirst = entry({
    model: "*",
    provider: "openai",
    real_model: "claude-3-opus",
    status: "pinned",
  })
  const entries2 = [pinnedFirst, entry({ model: "*", provider: "openai", real_model: "gpt-4o" })]
  const active2 = selectActiveTarget(entries2, "*")
  assert.equal(active2, pinnedFirst)
  assert.equal(isActiveFirstTarget(r, active2, entries2), true, "wildcard passthrough → green")
}

// ── empty targets / no route entries ─────────────────────────────────
{
  const r = route("gpt-4o", [])
  const orphan = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  assert.equal(
    isActiveFirstTarget(r, orphan, [orphan]),
    false,
    "empty target list can never be the first target",
  )
  assert.equal(activeTargetDotColor(r, undefined, undefined, []), null, "no active → no dot")
}

// ── expired takes priority over green/orange ─────────────────────────
{
  const r = route("gpt-4o", [target("openai", "gpt-4o")])
  const expired = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "expired",
    age_secs: 9999,
  })
  const entries = [expired]
  // Even though the expired entry IS the first target, expired renders gray.
  assert.equal(activeTargetDotColor(r, expired, undefined, entries), "bg-gray-500")
  assert.equal(
    isActiveFirstTarget(r, expired, entries),
    true,
    "identity check is independent of expiry (the cell orders expired first)",
  )

  // An expired pin must also win over a first-target match in the dot color.
  const notFirst = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    status: "expired",
    age_secs: 9999,
  })
  assert.equal(activeTargetDotColor(r, notFirst, undefined, entries), "bg-gray-500")
}

// ── fill-first stays emerald and is not affected by sticky logic ─────
{
  const r = route("gpt-4o", [target("openai", "gpt-4o")])
  const fill = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    strategy: "fill_first",
    fill_first_position: 0,
    status: "idle",
  })
  // fill-first target (sticky undefined) → emerald, regardless of targets.
  assert.equal(selectFillFirstTarget([fill], "gpt-4o"), fill)
  assert.equal(activeTargetDotColor(r, undefined, fill, [fill]), "bg-emerald-500")

  // If a sticky pin coexists, fill-first does not override it (the cell
  // prefers sticky) — the color then follows the sticky entry.
  const sticky = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  assert.equal(activeTargetDotColor(r, sticky, fill, [sticky, fill]), "bg-green-500")
}

// ── object identity (not field equality) is the discriminator ────────
{
  const r = route("gpt-4o", [target("openai", "gpt-4o"), target("openai", "gpt-4o")])
  // Two DTOs with byte-identical data; only the first (config-first target's)
  // entry may count as the first target.
  const firstDto = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  const secondDto = entry({
    model: "gpt-4o",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  const entries = [firstDto, secondDto]
  assert.equal(
    isActiveFirstTarget(r, firstDto, entries),
    true,
    "the first DTO is the first target even when a duplicate shares its data",
  )
  assert.equal(
    isActiveFirstTarget(r, secondDto, entries),
    false,
    "a duplicate with identical data but later position is not the first target",
  )
}

// ── mixed entries from multiple routes: the lookup is model-scoped, so
//    one route's pin never cross-attributes to another on the same
//    provider (and vice versa) ────────────────────────────────────────
{
  const a = route("gpt-4o", [target("openai", "gpt-4o"), target("anthropic", "claude-3")])
  const b = route("gpt-4o-vision", [target("openai", "gpt-4o")])
  const aFirst = entry({ model: "gpt-4o", provider: "openai", real_model: "gpt-4o" })
  const aPinned = entry({
    model: "gpt-4o",
    provider: "anthropic",
    real_model: "claude-3",
    status: "pinned",
  })
  const bPinned = entry({
    model: "gpt-4o-vision",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
  })
  // Interleave both routes' DTOs to prove the per-route lookup is scoped by
  // model, not by array position or provider. B's DTO is field-identical to
  // A's first DTO (provider openai / real_model gpt-4o) except for `model`.
  const entries = [bPinned, aFirst, aPinned]

  // Route A: pin landed on its second target → orange.
  assert.equal(selectActiveTarget(entries, "gpt-4o"), aPinned)
  assert.equal(isActiveFirstTarget(a, aPinned, entries), false)
  assert.equal(activeTargetDotColor(a, aPinned, undefined, entries), "bg-orange-500")

  // Route B: pin on its only target → green, and A's pin never leaks in.
  assert.equal(selectActiveTarget(entries, "gpt-4o-vision"), bPinned)
  assert.equal(isActiveFirstTarget(b, bPinned, entries), true)
  assert.equal(activeTargetDotColor(b, bPinned, undefined, entries), "bg-green-500")

  // A route with no pin at all gets no dot, even with B's pin nearby.
  assert.equal(selectActiveTarget(entries, "gpt-4o-turbo"), undefined)
  assert.equal(activeTargetDotColor(a, undefined, undefined, entries), null)
}

// ── wildcard multi-pin: newest pinned entry wins (per request model) ──
{
  const firstDto = entry({
    model: "*",
    provider: "openai",
    real_model: "gpt-4o",
    status: "pinned",
    age_secs: 100,
  })
  const newestDto = entry({
    model: "*",
    provider: "openai",
    real_model: "gpt-4.1",
    status: "pinned",
    age_secs: 5,
  })
  assert.equal(
    selectActiveTarget([firstDto, newestDto], "*"),
    newestDto,
    "most recently recorded pin is the active entry",
  )
  assert.equal(
    selectActiveTarget([newestDto, firstDto], "*"),
    newestDto,
    "age, not array order, picks the active entry",
  )
}

console.log("activeTarget.test: all assertions passed")

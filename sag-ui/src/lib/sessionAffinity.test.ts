import assert from "node:assert/strict"
import type { SessionAffinityEntry } from "../types/api.ts"
import {
  errorForModel,
  isPinActive,
  mergeSessionEntries,
  shortSessionKey,
  sortPins,
} from "./sessionAffinity.ts"

// The popover shows only the leading characters of a hashed session key; the
// full 32-hex value stays in the `title`/DELETE body. These tests pin the
// truncation so a wrong slice can't send a truncated key back to
// `DELETE /api/session-affinity`.

// ── default prefix length ──────────────────────────────────────────
{
  const key = "0123456789abcdef0123456789abcdef"
  assert.equal(shortSessionKey(key), "01234567", "8 leading hex chars")
  assert.equal(shortSessionKey(key).length, 8)
}

// ── short / empty keys stay intact ─────────────────────────────────
{
  assert.equal(shortSessionKey(""), "", "empty key renders as empty string")
  assert.equal(shortSessionKey("abc"), "abc", "a key shorter than the prefix is not padded")
}

// ── custom length is honoured (never negative) ─────────────────────
{
  const key = "0123456789abcdef0123456789abcdef"
  assert.equal(shortSessionKey(key, 4), "0123")
  assert.equal(shortSessionKey(key, 0), "")
  assert.equal(shortSessionKey(key, -3), "", "a negative length yields an empty string")
  assert.equal(shortSessionKey(key, 64), key, "length beyond the key returns the whole key")
}

// ── pin liveness ───────────────────────────────────────────────────
// `expires_in_secs === null` means the route TTL is 0 (never expires), a
// positive countdown is live, and a `<= 0` value is an expired residue the
// popover greys out. These tests pin that boundary so a zero-countdown pin
// cannot be shown as active.

function entry(
  key: string,
  expires_in_secs: number | null,
  model = "gpt-4o",
): SessionAffinityEntry {
  return {
    key,
    model,
    source: "meta",
    provider: "openai",
    real_model: "gpt-4o",
    route_model: "gpt-4o",
    age_secs: 12,
    ttl_secs: expires_in_secs === null ? 0 : 3600,
    expires_in_secs,
  }
}

{
  assert.equal(isPinActive(entry("never", null)), true, "null expires_in_secs never expires")
  assert.equal(isPinActive(entry("fresh", 3600)), true, "a positive countdown is active")
  assert.equal(isPinActive(entry("last", 1)), true, "one second left is still active")
  assert.equal(isPinActive(entry("zero", 0)), false, "a zero countdown is not active")
  assert.equal(isPinActive(entry("stale", -30)), false, "a negative countdown is not active")
}

// ── ordering: active group first, stable inside each group ─────────
{
  const input = [
    entry("expired-a", -5),
    entry("active-a", 100),
    entry("never", null),
    entry("expired-b", 0),
    entry("active-b", 1),
  ]
  const sorted = sortPins(input)

  assert.deepEqual(
    sorted.map((e) => e.key),
    ["active-a", "never", "active-b", "expired-a", "expired-b"],
    "active pins keep their relative order ahead of the expired ones",
  )
  assert.deepEqual(
    input.map((e) => e.key),
    ["expired-a", "active-a", "never", "expired-b", "active-b"],
    "sortPins does not mutate its input",
  )
  assert.notEqual(sorted, input, "a new array is returned")
}

// ── degenerate inputs ──────────────────────────────────────────────
{
  assert.deepEqual(sortPins([]), [], "an empty list stays empty")

  const allExpired = [entry("x", -1), entry("y", 0)]
  assert.deepEqual(
    sortPins(allExpired).map((e) => e.key),
    ["x", "y"],
    "an all-expired list keeps the original order",
  )

  const allActive = [entry("p", 10), entry("q", null)]
  assert.deepEqual(
    sortPins(allActive).map((e) => e.key),
    ["p", "q"],
    "an all-active list keeps the original order",
  )
}

// ── merging a one-model response into the shared list ──────────────
// Every row's popover reads one shared store, so `fetchEntries({ model })`
// must only swap that model's bucket. Replacing the whole list dropped the
// other rows' pins and made a still-pinned row render "no pins".

{
  const prev = [entry("a1", 100, "model-a"), entry("b1", 100, "model-b")]
  const incoming = [entry("a2", 100, "model-a")]
  const merged = mergeSessionEntries(prev, incoming, "model-a")

  assert.deepEqual(
    merged.map((e) => e.key),
    ["b1", "a2"],
    "only the fetched model's bucket is replaced; the other rows survive",
  )
  assert.notEqual(merged, prev, "a new array is returned")
  assert.notEqual(merged, incoming, "the incoming array is not reused")
}

{
  const prev = [entry("a1", 100, "model-a"), entry("a2", 100, "model-a")]
  const merged = mergeSessionEntries(prev, [entry("a3", 100, "model-a")], "model-a")

  assert.deepEqual(
    merged.map((e) => e.key),
    ["a3"],
    "a re-fetch of the same model drops its stale entries",
  )
}

{
  // An empty bucket is meaningful: clearing a model space must not resurrect
  // the entries the previous snapshot had for it.
  const prev = [entry("a1", 100, "model-a"), entry("b1", 100, "model-b")]
  assert.deepEqual(
    mergeSessionEntries(prev, [], "model-a").map((e) => e.key),
    ["b1"],
    "an empty response empties that model's bucket only",
  )
}

{
  const prev = [entry("a1", 100, "model-a")]
  const incoming = [entry("b1", 100, "model-b")]

  assert.deepEqual(
    mergeSessionEntries(prev, incoming).map((e) => e.key),
    ["b1"],
    "omitting the model replaces the whole list",
  )
  assert.deepEqual(
    mergeSessionEntries(prev, incoming, undefined).map((e) => e.key),
    ["b1"],
    "an explicit undefined is a full-space fetch too",
  )
}

{
  const prev = [entry("a1", 100, "model-a"), entry("b1", 100, "model-b")]
  const incoming = [entry("a2", 100, "model-a")]
  const prevSnapshot = [...prev]
  const incomingSnapshot = [...incoming]

  mergeSessionEntries(prev, incoming, "model-a")
  mergeSessionEntries(prev, incoming)

  assert.deepEqual(prev, prevSnapshot, "mergeSessionEntries does not mutate prev")
  assert.deepEqual(incoming, incomingSnapshot, "mergeSessionEntries does not mutate incoming")
}

// ── scoping a fetch failure to its own row ─────────────────────────
// The store is shared by every row, so a failed `{ model: B }` fetch must
// only be readable by row B. These tests pin that a matching scope yields the
// message and everything else yields `null` (so row A keeps its list).

{
  const failure = { scope: "model-b", message: "boom" }

  assert.equal(errorForModel(failure, "model-b"), "boom", "the failing row reads its message")
  assert.equal(
    errorForModel(failure, "model-a"),
    null,
    "another row never reads someone else's failure",
  )
}

{
  // A full-space fetch has no scope; no row can claim its message.
  const failure = { message: "boom" }

  assert.equal(errorForModel(failure, "model-a"), null, "an unscoped failure belongs to no row")
  assert.equal(errorForModel({ scope: undefined, message: "boom" }, ""), null)
}

{
  assert.equal(errorForModel(null, "model-a"), null, "no stored error yields null")
  assert.equal(
    errorForModel({ scope: "", message: "boom" }, ""),
    "boom",
    "an empty scope matches an empty model (strict equality, no coercion)",
  )
}

console.log("# sessionAffinity.test: all assertions passed")

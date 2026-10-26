import assert from "node:assert/strict"
import type { SessionAffinityEntry } from "../types/api.ts"
import {
  errorForModel,
  isPinActive,
  loadingForModel,
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

// ── reading one scope's fetch failure ──────────────────────────────
// The store is shared by every row and keeps failures keyed by fetch scope
// (`""` is the full-space fetch), so a failed `{ model: B }` fetch must only be
// readable by row B — row A keeps its list instead of rendering B's message.

{
  const errors = { "model-b": "boom" }

  assert.equal(errorForModel(errors, "model-b"), "boom", "the failing row reads its message")
  assert.equal(
    errorForModel(errors, "model-a"),
    null,
    "another row never reads someone else's failure",
  )
  assert.equal(errorForModel(errors, ""), null, "a row never reads the full-space failure")
}

{
  // Two rows may fail at the same time: each keeps its own message.
  const errors = { "model-a": "a failed", "model-b": "b failed" }

  assert.equal(errorForModel(errors, "model-a"), "a failed", "each scope keeps its own message")
  assert.equal(errorForModel(errors, "model-b"), "b failed", "a later failure does not overwrite")
}

{
  assert.equal(errorForModel({}, "model-a"), null, "an empty map yields null for every row")
  assert.equal(errorForModel({}, ""), null, "an empty map yields null for the full-space scope")
}

{
  assert.equal(
    errorForModel({ "model-a": "boom" }, "model-c"),
    null,
    "a model without a key yields null (no fallback to another scope)",
  )
  assert.equal(
    errorForModel({ "": "full failed" }, ""),
    "full failed",
    "the empty key is the full-space scope, matched as a plain key",
  )
  assert.equal(
    errorForModel({ "": "full failed" }, "model-a"),
    null,
    "a model row never reads the full-space failure",
  )
}

// ── prototype keys are not failures ────────────────────────────────
// The map is a plain object, so a bare `errors[model]` walks the prototype
// chain: a route model literally named `constructor` / `toString` would render
// a `Function` as its failure text. Only own keys are real failures.

{
  for (const inherited of ["constructor", "toString", "hasOwnProperty", "__proto__"]) {
    assert.equal(
      errorForModel({}, inherited),
      null,
      `an empty map yields null for the inherited key ${inherited}`,
    )
  }
}

{
  assert.equal(
    errorForModel({ constructor: "boom" }, "constructor"),
    "boom",
    "an own key named constructor is a real failure",
  )
  assert.equal(
    errorForModel({ toString: "boom" }, "toString"),
    "boom",
    "an own key named toString is a real failure",
  )
  assert.equal(
    errorForModel({ constructor: "boom" }, "model-a"),
    null,
    "an own prototype-named key is not readable by another row",
  )
}

// ── reading one scope's in-flight flag ─────────────────────────────
// The same shared map and the same own-property guard as `errorForModel`: a
// bare `loading[model] ?? false` walks the prototype chain, so a route model
// named `constructor` would read a `Function` and spin forever.
{
  const loading = { "model-b": true, "model-c": false }

  assert.equal(loadingForModel(loading, "model-b"), true, "the fetching row is in flight")
  assert.equal(loadingForModel(loading, "model-c"), false, "an explicit false stays false")
  assert.equal(
    loadingForModel(loading, "model-a"),
    false,
    "a row without a key is not in flight (another row's request does not spin it)",
  )
  assert.equal(loadingForModel(loading, ""), false, "the full-space scope has no key here")
}

{
  for (const inherited of ["constructor", "toString", "__proto__"]) {
    assert.equal(
      loadingForModel({}, inherited),
      false,
      `an empty map is not in flight for the inherited key ${inherited}`,
    )
  }
}

{
  assert.equal(
    loadingForModel({ constructor: true }, "constructor"),
    true,
    "an own key named constructor is a real in-flight flag",
  )
  assert.equal(
    loadingForModel({ constructor: true }, "model-a"),
    false,
    "an own prototype-named key does not spin another row",
  )
}

console.log("# sessionAffinity.test: all assertions passed")

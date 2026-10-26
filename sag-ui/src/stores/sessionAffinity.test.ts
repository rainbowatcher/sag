import assert from "node:assert/strict"
import { createPinia, setActivePinia } from "pinia"
import { api } from "../lib/client.ts"
import type { SessionAffinityEntry } from "../types/api.ts"
import { useSessionAffinityStore } from "./sessionAffinity.ts"

// The session-affinity store is shared by every route row, so its fetch
// contract is per scope: one row's `{ model }` fetch must only replace that
// model's slice, a failure must be readable by that row alone, and a slow
// response must never overwrite a newer one. These tests pin those rules.
//
// The network layer is axios (`lib/client.ts`), not `fetch`, so the shared
// instance's `get` is replaced by a queue of deferred promises: every test
// decides the settlement order itself, which is what makes the staleness
// cases expressible at all.

interface Deferred {
  url: string
  params: Record<string, unknown> | undefined
  resolve: (value: { data: { entries: SessionAffinityEntry[] } }) => void
  reject: (reason: unknown) => void
}

const pending: Deferred[] = []
let cursor = 0

api.get = ((url: string, config?: { params?: Record<string, unknown> }) =>
  new Promise((resolve, reject) => {
    pending.push({ url, params: config?.params, resolve, reject })
  })) as unknown as typeof api.get

/** The next request the store issued, in issue order. */
function nextCall(): Deferred {
  const call = pending[cursor]
  cursor += 1
  assert.ok(call, "expected the store to have issued another request")
  return call
}

function entry(key: string, model: string): SessionAffinityEntry {
  return {
    key,
    model,
    source: "meta",
    provider: "openai",
    real_model: "gpt-4o",
    route_model: "gpt-4o",
    age_secs: 12,
    ttl_secs: 3600,
    expires_in_secs: 3600,
  }
}

function freshStore() {
  setActivePinia(createPinia())
  return useSessionAffinityStore()
}

function keysOf(entries: SessionAffinityEntry[]): string[] {
  return entries.map((e) => e.key)
}

// ── a scoped fetch only swaps its own bucket ───────────────────────
{
  const store = freshStore()

  const full = store.fetchEntries()
  const fullCall = nextCall()
  assert.equal(fullCall.url, "/api/session-affinity", "the store calls the list endpoint")
  assert.deepEqual(fullCall.params, {}, "a full-space fetch sends no model filter")
  fullCall.resolve({ data: { entries: [entry("a1", "model-a"), entry("b1", "model-b")] } })
  await full

  assert.deepEqual(keysOf(store.entries), ["a1", "b1"], "a full-space fetch replaces the list")

  const scoped = store.fetchEntries({ model: "model-a" })
  const scopedCall = nextCall()
  assert.deepEqual(scopedCall.params, { model: "model-a" }, "the model filter is forwarded")
  scopedCall.resolve({ data: { entries: [entry("a2", "model-a")] } })
  await scoped

  assert.deepEqual(
    keysOf(store.entries),
    ["b1", "a2"],
    "only the fetched model's bucket is replaced; the other rows survive",
  )
  assert.deepEqual(store.loading, {}, "the in-flight flag is cleared after a success")
  assert.deepEqual(store.errors, {}, "a success records no failure")
}

// ── a failure is scoped and keeps the last good list ───────────────
// A failed refresh is not an empty result: the rows must keep rendering the
// pins they had, and only the failing scope carries the message.
{
  const store = freshStore()

  const first = store.fetchEntries({ model: "model-a" })
  nextCall().resolve({ data: { entries: [entry("a1", "model-a")] } })
  await first

  const failing = store.fetchEntries({ model: "model-b" })
  const failingCall = nextCall()
  assert.equal(store.loading["model-b"], true, "the failing scope is marked in flight")
  failingCall.reject(new Error("boom"))
  await failing

  assert.deepEqual(keysOf(store.entries), ["a1"], "a failed fetch does not clear the entries")
  assert.equal(store.errors["model-b"], "boom", "the failure is stored under its own scope")
  assert.equal(store.errors["model-a"], undefined, "another scope records no failure")
  assert.deepEqual(store.loading, {}, "the in-flight flag is cleared after a failure")
}

// ── a retry keeps the previous failure until it settles ────────────
// Clearing `errors[scope]` when the retry starts made the row flash its
// loading/empty state; the message must survive until the retry succeeds.
{
  const store = freshStore()

  const failing = store.fetchEntries({ model: "model-a" })
  nextCall().reject(new Error("boom"))
  await failing
  assert.equal(store.errors["model-a"], "boom", "the first attempt failed")

  const retry = store.fetchEntries({ model: "model-a" })
  const retryCall = nextCall()
  assert.equal(store.errors["model-a"], "boom", "starting a retry does not clear the failure")
  assert.equal(store.loading["model-a"], true, "the retry is in flight")

  retryCall.resolve({ data: { entries: [entry("a2", "model-a")] } })
  await retry

  assert.equal(store.errors["model-a"], undefined, "a successful retry clears its own failure")
  assert.deepEqual(keysOf(store.entries), ["a2"], "the successful retry is merged in")
}

// ── clearing a failure only drops the matching key ─────────────────
{
  const store = freshStore()

  const failingA = store.fetchEntries({ model: "model-a" })
  nextCall().reject(new Error("a failed"))
  await failingA

  const failingB = store.fetchEntries({ model: "model-b" })
  nextCall().reject(new Error("b failed"))
  await failingB

  assert.equal(store.errors["model-a"], "a failed", "scope a keeps its message")
  assert.equal(store.errors["model-b"], "b failed", "scope b keeps its message")

  // A successful refetch of scope a is the only thing that clears its failure.
  const okA = store.fetchEntries({ model: "model-a" })
  nextCall().resolve({ data: { entries: [] } })
  await okA

  assert.equal(store.errors["model-a"], undefined, "scope a's failure is cleared by its success")
  assert.equal(store.errors["model-b"], "b failed", "scope b's failure survives")
}

// ── a stale response never writes its scope ────────────────────────
// One monotonic token per scope: only the latest in-flight fetch of a scope
// may write that scope's state, so a slow earlier request cannot overwrite a
// newer one.
{
  const store = freshStore()

  const first = store.fetchEntries({ model: "model-a" })
  const second = store.fetchEntries({ model: "model-a" })
  const firstCall = nextCall()
  const secondCall = nextCall()

  secondCall.resolve({ data: { entries: [entry("new", "model-a")] } })
  await second
  firstCall.resolve({ data: { entries: [entry("stale", "model-a")] } })
  await first

  assert.deepEqual(
    keysOf(store.entries),
    ["new"],
    "the newer response wins; the slower earlier one is dropped",
  )
}

{
  // A stale *failure* is dropped too — otherwise a slow 500 could paint an
  // error over a row that already refreshed successfully.
  const store = freshStore()

  const stale = store.fetchEntries({ model: "model-b" })
  const fresh = store.fetchEntries({ model: "model-b" })
  const staleCall = nextCall()
  const freshCall = nextCall()

  freshCall.reject(new Error("fresh failed"))
  await fresh
  staleCall.reject(new Error("stale failed"))
  await stale

  assert.equal(store.errors["model-b"], "fresh failed", "only the newest failure is recorded")
}

{
  // Final state alone does not prove a stale response kept its hands off the
  // maps: it must not write (or clear) its own scope's keys at all. A newer
  // fetch is left in flight so an unconditional write would be observable.
  const store = freshStore()

  const stale = store.fetchEntries({ model: "model-a" })
  const mid = store.fetchEntries({ model: "model-a" })
  const newest = store.fetchEntries({ model: "model-a" })
  const staleCall = nextCall()
  const midCall = nextCall()
  const newestCall = nextCall()

  midCall.resolve({ data: { entries: [entry("mid", "model-a")] } })
  await mid
  staleCall.resolve({ data: { entries: [entry("stale", "model-a")] } })
  await stale

  assert.deepEqual(
    keysOf(store.entries),
    [],
    "two stale successes write no entries while the newest fetch runs",
  )
  assert.equal(
    Object.hasOwn(store.loading, "model-a"),
    true,
    "a stale success does not clear the in-flight flag of a still-running fetch",
  )
  assert.equal(
    Object.hasOwn(store.errors, "model-a"),
    false,
    "a stale success does not touch the failures of its scope",
  )

  newestCall.resolve({ data: { entries: [entry("newest", "model-a")] } })
  await newest

  assert.deepEqual(keysOf(store.entries), ["newest"], "the latest fetch wins in the end")
  assert.deepEqual(store.loading, {}, "no scope is left in flight")
  assert.deepEqual(store.errors, {}, "no failure is left behind")
}

{
  // The stale failure variant: it must not add an error key either.
  const store = freshStore()

  const stale = store.fetchEntries({ model: "model-b" })
  const fresh = store.fetchEntries({ model: "model-b" })
  const staleCall = nextCall()
  const freshCall = nextCall()

  freshCall.resolve({ data: { entries: [entry("b1", "model-b")] } })
  await fresh
  staleCall.reject(new Error("stale failed"))
  await stale

  assert.equal(
    Object.hasOwn(store.errors, "model-b"),
    false,
    "a stale failure does not write the failures of its scope",
  )
  assert.equal(
    Object.hasOwn(store.loading, "model-b"),
    false,
    "a stale failure does not leave its scope in flight",
  )
  assert.deepEqual(keysOf(store.entries), ["b1"], "the newer response's entries survive")
}

// ── the in-flight flag is per scope ────────────────────────────────
// The popover reads only its own row's flag: a slow fetch of row A must not
// turn row B's tooltip into a spinner.
{
  const store = freshStore()

  const a = store.fetchEntries({ model: "model-a" })
  const aCall = nextCall()

  assert.equal(store.loading["model-a"], true, "the fetching scope is in flight")
  assert.equal(
    Object.hasOwn(store.loading, "model-b"),
    false,
    "another scope is not in flight while this one is",
  )

  aCall.resolve({ data: { entries: [entry("a1", "model-a")] } })
  await a

  assert.equal(
    Object.hasOwn(store.loading, "model-a"),
    false,
    "the scope leaves the in-flight state",
  )
  assert.equal(Object.hasOwn(store.loading, "model-b"), false, "the other scope was never marked")
}

// ── concurrent scopes never cross-write ────────────────────────────
{
  const store = freshStore()

  const a = store.fetchEntries({ model: "model-a" })
  const b = store.fetchEntries({ model: "model-b" })
  const aCall = nextCall()
  const bCall = nextCall()

  // Resolve out of issue order: b settles first, then a fails.
  bCall.resolve({ data: { entries: [entry("b1", "model-b")] } })
  await b
  aCall.reject(new Error("a failed"))
  await a

  assert.deepEqual(keysOf(store.entries), ["b1"], "b's entries land, a's failure adds none")
  assert.equal(store.errors["model-a"], "a failed", "a's failure stays in a's scope")
  assert.equal(store.errors["model-b"], undefined, "b's success leaves no failure behind")
  assert.deepEqual(store.loading, {}, "both scopes leave the in-flight state")
}

console.log("# stores/sessionAffinity.test: all assertions passed")

import assert from "node:assert/strict"
import type { InflightSnapshot } from "../types/api.ts"
import {
  activeRouteCounts,
  applySnapshot,
  emptyInflightState,
  markConnected,
  markDisconnected,
  parseInflightSnapshot,
  resolveSseUrl,
} from "./inflight.ts"

// The routes page mirrors the `GET /api/inflight` SSE stream. The backend sends
// a FULL snapshot on connect and on every change, so the store is replace-only.
// These tests pin the two invariants that matter most: a malformed frame must
// never blank the panel, and a dropped stream must keep (never zero) the last
// snapshot while flagging it stale.

function snapshot(partial: Partial<InflightSnapshot> = {}): InflightSnapshot {
  return { seq: 1, total: 0, routes: [], ...partial }
}

// ── frame parsing ──────────────────────────────────────────────────
{
  const parsed = parseInflightSnapshot(
    '{"seq":7,"total":3,"routes":[{"model_name":"gpt-4o","count":2},{"model_name":"claude","count":1}]}',
  )
  assert.deepEqual(parsed, {
    seq: 7,
    total: 3,
    routes: [
      { model_name: "gpt-4o", count: 2 },
      { model_name: "claude", count: 1 },
    ],
  })
}

{
  // Routes are re-sorted descending so badge order never depends on the
  // backend's sort being stable.
  const parsed = parseInflightSnapshot(
    '{"seq":1,"total":6,"routes":[{"model_name":"a","count":1},{"model_name":"b","count":5}]}',
  )
  assert.deepEqual(parsed?.routes, [
    { model_name: "b", count: 5 },
    { model_name: "a", count: 1 },
  ])
}

{
  assert.equal(parseInflightSnapshot("not json"), null, "invalid JSON is rejected")
  assert.equal(parseInflightSnapshot(""), null)
  assert.equal(parseInflightSnapshot("null"), null, "a JSON null is rejected")
  assert.equal(parseInflightSnapshot("[1,2]"), null, "a JSON array is rejected")
  assert.equal(parseInflightSnapshot('{"total":3}'), null, "a snapshot without seq is rejected")
  assert.equal(parseInflightSnapshot('{"seq":"1"}'), null, "a non-numeric seq is rejected")
}

{
  // Tolerate junk inside a frame: unknown fields ignored, unusable route entries
  // dropped, and `total` derived from the routes when the field is missing.
  const parsed = parseInflightSnapshot(
    '{"seq":2,"routes":[{"model_name":"ok","count":4},{"model_name":"","count":1},' +
      '{"count":9},{"model_name":"neg","count":-1},{"model_name":"bad","count":"2"}],"extra":true}',
  )
  assert.deepEqual(parsed, { seq: 2, total: 4, routes: [{ model_name: "ok", count: 4 }] })
}

{
  const parsed = parseInflightSnapshot(
    '{"seq":3,"total":0,"routes":[{"model_name":"  padded  ","count":1}]}',
  )
  assert.equal(parsed?.routes[0].model_name, "padded", "model names are trimmed")
}

{
  const parsed = parseInflightSnapshot('{"seq":4,"total":2.9,"routes":[]}')
  assert.equal(parsed?.total, 2, "counts are floored to integers")
}

{
  const parsed = parseInflightSnapshot('{"seq":5,"total":0,"routes":[]}')
  assert.deepEqual(parsed, { seq: 5, total: 0, routes: [] }, "an empty snapshot is valid")
}

// ── snapshot replacement ───────────────────────────────────────────
{
  const next = snapshot({ seq: 2, total: 1, routes: [{ model_name: "gpt-4o", count: 1 }] })
  const state = applySnapshot({ snapshot: null, connected: false, stale: true }, next)
  assert.equal(state.snapshot, next, "the whole snapshot replaces the previous one")
  assert.equal(state.connected, true, "a frame proves the stream is live")
  assert.equal(state.stale, false, "a fresh snapshot clears the stale flag")
}

{
  // A second snapshot replaces, never merges: a route that dropped to zero must
  // disappear from the list rather than keep its previous count.
  const first = snapshot({ seq: 1, total: 3, routes: [{ model_name: "a", count: 3 }] })
  const second = snapshot({ seq: 2, total: 0, routes: [] })
  let state = applySnapshot(emptyInflightState(), first)
  state = applySnapshot(state, second)
  assert.deepEqual(state.snapshot, second)
  assert.equal(state.snapshot?.total, 0)
}

// ── connect / drop transitions ─────────────────────────────────────
{
  const held = snapshot({ seq: 1, total: 2, routes: [{ model_name: "a", count: 2 }] })
  const connected = markConnected({ snapshot: held, connected: false, stale: true })
  assert.equal(connected.connected, true, "an open stream is marked connected")
  assert.equal(connected.snapshot, held, "opening does not touch the snapshot")
  assert.equal(connected.stale, true, "opening alone does not confirm the numbers")
}

{
  const held = snapshot({ seq: 1, total: 2, routes: [{ model_name: "a", count: 2 }] })
  const dropped = markDisconnected({ snapshot: held, connected: true, stale: false })
  assert.equal(dropped.connected, false)
  assert.equal(dropped.snapshot, held, "the last snapshot is kept, never zeroed")
  assert.equal(dropped.stale, true, "the kept snapshot is flagged stale")
}

{
  const dropped = markDisconnected(emptyInflightState())
  assert.equal(dropped.snapshot, null)
  assert.equal(dropped.stale, false, "nothing to be stale about before the first frame")
}

// ── badge filtering ────────────────────────────────────────────────
{
  assert.deepEqual(
    activeRouteCounts([
      { model_name: "a", count: 0 },
      { model_name: "b", count: 2 },
    ]),
    [{ model_name: "b", count: 2 }],
    "only routes with traffic get a badge",
  )
  assert.deepEqual(activeRouteCounts([]), [])
}

// ── stream URL ─────────────────────────────────────────────────────
{
  assert.equal(resolveSseUrl("", "/api/inflight"), "/api/inflight", "same-origin gateway")
  assert.equal(resolveSseUrl(undefined, "/api/inflight"), "/api/inflight")
  assert.equal(
    resolveSseUrl("http://localhost:37210", "/api/inflight"),
    "http://localhost:37210/api/inflight",
    "a separately served gateway keeps its origin",
  )
  assert.equal(
    resolveSseUrl("http://localhost:37210/", "/api/inflight"),
    "http://localhost:37210/api/inflight",
    "a trailing slash is not doubled",
  )
}

console.log("# inflight.test: all assertions passed")

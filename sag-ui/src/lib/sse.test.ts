import assert from "node:assert/strict"
import {
  applySnapshot,
  emptyInflightState,
  markDisconnected,
  parseInflightSnapshot,
} from "./inflight.ts"
import {
  SSE_MAX_DATA_LINES,
  SSE_MAX_LINE_LENGTH,
  SseFrameLimitError,
  SseHttpError,
  createSseClient,
  createSseParser,
  headersFromDefaults,
  isAuthFailure,
  nextRetryDelay,
} from "./sse.ts"

// The gateway streams `GET /api/inflight` as SSE (`event: inflight`, `: ping`
// heartbeats every 15s). `EventSource` cannot attach the auth header, so the
// client reads the byte stream through `fetch` — which means the parser has to
// be correct on chunk boundaries, not just on whole frames.

// ── single frame ───────────────────────────────────────────────────
{
  const parser = createSseParser()
  const events = parser.push('event: inflight\ndata: {"seq":1,"total":0,"routes":[]}\n\n')
  assert.equal(events.length, 1)
  assert.equal(events[0].event, "inflight")
  assert.equal(events[0].data, '{"seq":1,"total":0,"routes":[]}')
  assert.equal(events[0].id, null)
}

// ── cross-chunk framing ────────────────────────────────────────────
{
  const parser = createSseParser()
  assert.deepEqual(parser.push("event: infl"), [], "a partial line yields nothing")
  assert.deepEqual(parser.push('ight\ndata: {"seq":2'), [], "a partial data line yields nothing")
  const events = parser.push(',"total":1,"routes":[]}\n\n')
  assert.equal(events.length, 1, "the frame completes only on the blank line")
  assert.equal(events[0].event, "inflight")
  assert.equal(events[0].data, '{"seq":2,"total":1,"routes":[]}')
}

// ── one chunk carrying several frames ──────────────────────────────
{
  const parser = createSseParser()
  const events = parser.push(
    'event: inflight\ndata: {"seq":1,"total":1,"routes":[]}\n\n' +
      'event: inflight\ndata: {"seq":2,"total":2,"routes":[]}\n\n',
  )
  assert.equal(events.length, 2)
  assert.equal(events[0].data, '{"seq":1,"total":1,"routes":[]}')
  assert.equal(events[1].data, '{"seq":2,"total":2,"routes":[]}')
}

// ── heartbeat comments are not events ──────────────────────────────
{
  const parser = createSseParser()
  assert.deepEqual(parser.push(": ping\n\n"), [], "a comment-only frame dispatches nothing")
  const events = parser.push(': keep-alive\ndata: {"seq":3,"total":0,"routes":[]}\n\n')
  assert.equal(events.length, 1, "a comment before data does not block the frame")
  assert.equal(events[0].data, '{"seq":3,"total":0,"routes":[]}')
}

// ── CRLF line endings ──────────────────────────────────────────────
{
  const parser = createSseParser()
  assert.deepEqual(parser.push('event: inflight\r\ndata: {"seq":1}\r'), [], "a split CRLF waits")
  const events = parser.push("\n\r\n")
  assert.equal(events.length, 1)
  assert.equal(events[0].event, "inflight")
  assert.equal(events[0].data, '{"seq":1}')
}

// ── multi-line data and default event name ─────────────────────────
{
  const parser = createSseParser()
  const events = parser.push("data: line one\ndata: line two\n\n")
  assert.equal(events[0].event, "message", "an unnamed frame defaults to message")
  assert.equal(events[0].data, "line one\nline two", "data lines join with a newline")
}

// ── id field, and no dispatch without data ─────────────────────────
{
  const parser = createSseParser()
  assert.deepEqual(parser.push("id: 42\n\n"), [], "an id-only frame dispatches nothing")
  const events = parser.push("data: payload\n\n")
  assert.equal(events[0].id, "42", "the last id is carried on later frames")
}

// ── flush: a trailing frame with no closing blank line ─────────────
{
  const parser = createSseParser()
  assert.deepEqual(parser.push('event: inflight\ndata: {"seq":9}'), [], "still buffered")
  const events = parser.flush()
  assert.equal(events.length, 1, "flush dispatches the trailing frame")
  assert.equal(events[0].data, '{"seq":9}')
  assert.deepEqual(parser.flush(), [], "flush is idempotent")
}
// A trailing lone CR can no longer be the first half of a CRLF at end of stream.
{
  const parser = createSseParser()
  parser.push('event: inflight\ndata: {"seq":9}\r')
  const events = parser.flush()
  assert.equal(events.length, 1, "the frame still dispatches")
  assert.equal(events[0].data, '{"seq":9}', "the stray CR is not part of the value")
}
{
  const parser = createSseParser()
  parser.push("event: inflight\r")
  const events = parser.push("\ndata: x")
  assert.equal(events.length, 0)
  const tail = parser.flush()
  assert.equal(tail.length, 1)
  assert.equal(tail[0].event, "inflight", "a CRLF split across chunks still names the frame")
  assert.equal(tail[0].data, "x")
}

// ── field-value parsing details ────────────────────────────────────
{
  const parser = createSseParser()
  const events = parser.push("data:no-space\n\n")
  assert.equal(events[0].data, "no-space", "the leading space after ':' is optional")
}
{
  const parser = createSseParser()
  const events = parser.push("retry: 5000\ndata: x\n\n")
  assert.equal(events.length, 1, "retry is ignored, data still dispatches")
  assert.equal(events[0].data, "x")
}

// ── frame caps: an oversized frame is a broken stream, not a memory sink ──
{
  const parser = createSseParser()
  assert.throws(
    () => parser.push(`data: ${"x".repeat(SSE_MAX_LINE_LENGTH)}\n\n`),
    SseFrameLimitError,
    "a terminated line over the cap fails the frame",
  )
}
{
  const parser = createSseParser()
  assert.throws(
    () => parser.push("x".repeat(SSE_MAX_LINE_LENGTH + 1)),
    SseFrameLimitError,
    "a line that never terminates trips the cap through the buffer alone",
  )
}
{
  const parser = createSseParser()
  const line = `data: ${"x".repeat(1024)}\n`
  assert.throws(
    () => {
      for (let i = 0; i < 300; i++) parser.push(line)
    },
    SseFrameLimitError,
    "the data of one frame is capped, however it is chunked",
  )
}
{
  const parser = createSseParser()
  assert.throws(
    () => {
      // Empty data lines barely grow the length sum but multiply the line count.
      for (let i = 0; i < SSE_MAX_DATA_LINES + 1; i++) parser.push("data:\n")
    },
    SseFrameLimitError,
    "the number of data lines in one frame is capped",
  )
}

// ── backoff schedule ───────────────────────────────────────────────
{
  assert.equal(nextRetryDelay(0), 1000, "first retry waits one base delay")
  assert.equal(nextRetryDelay(1), 2000)
  assert.equal(nextRetryDelay(2), 4000)
  assert.equal(nextRetryDelay(3), 8000)
  assert.equal(nextRetryDelay(10), 30000, "the delay is clamped to the ceiling")
  assert.equal(nextRetryDelay(0, 500, 8000), 500, "the base delay is configurable")
  assert.equal(nextRetryDelay(-3), 1000, "a negative attempt falls back to the first step")
}

// ── auth header forwarding ─────────────────────────────────────────
{
  const headers = headersFromDefaults({ Authorization: "Bearer sk-1", "X-Trace": 7 })
  assert.deepEqual(headers, { Authorization: "Bearer sk-1" }, "only string values are forwarded")
  assert.deepEqual(headersFromDefaults(null), {}, "no defaults yields no headers")
  assert.deepEqual(
    headersFromDefaults({ Authorization: "" }),
    {},
    "an empty header value is dropped",
  )
}

// ── connection loop (mocked fetch + ReadableStream) ────────────────
// The parser tests above are synchronous; the loop below is what actually
// carries the auth header and recovers from a dropped stream.
const realFetch = globalThis.fetch

type Call = { url: string; headers: Record<string, string> }

/** Serve `chunks` then close the body, like a gateway that ends the stream. */
function streamResponse(chunks: string[]): Response {
  const encoder = new TextEncoder()
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk))
      controller.close()
    },
  })
  return new Response(body, { status: 200 })
}

/** Serve raw byte chunks then close the body (lets a test split a UTF-8 char). */
function byteResponse(chunks: Uint8Array[]): Response {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk)
      controller.close()
    },
  })
  return new Response(body, { status: 200 })
}

function stubFetch(responses: Response[]): Call[] {
  const calls: Call[] = []
  globalThis.fetch = (async (input: any, init: any) => {
    calls.push({ url: String(input), headers: { ...init?.headers } })
    const next = responses.shift()
    if (!next) throw new Error("no scripted response left")
    return next
  }) as typeof fetch
  return calls
}

/** Wait for `predicate()` to hold, polling on the microtask/macrotask queue. */
async function until(predicate: () => boolean, label: string): Promise<void> {
  for (let i = 0; i < 200; i++) {
    if (predicate()) return
    // oxlint-disable-next-line no-await-in-loop
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(`timed out waiting for ${label}`)
}

// events arrive, auth header is forwarded, backoff resets after a good stream
{
  const calls = stubFetch([
    streamResponse(['event: inflight\ndata: {"seq":1}\n\n', ": ping\n\n"]),
    streamResponse([]),
  ])
  const events: string[] = []
  let opens = 0
  let errors = 0
  const client = createSseClient({
    url: "http://gateway/api/inflight",
    // What the store really passes: verbatim axios defaults, which carry
    // `Accept: application/json, ...`.
    headers: headersFromDefaults({
      Accept: "application/json, text/plain, */*",
      Authorization: "Bearer sk-1",
    }),
    baseDelayMs: 5,
    onOpen: () => {
      opens += 1
    },
    onEvent: (event) => events.push(event.data),
    onError: () => {
      errors += 1
    },
  })
  client.start()
  assert.equal(client.isRunning(), true)
  await until(() => errors >= 2, "two dropped streams")
  client.stop()

  assert.deepEqual(events, ['{"seq":1}'], "only the inflight frame reaches the caller")
  assert.ok(opens >= 2, "the client reconnects after the stream ends")
  assert.ok(calls.length >= 2)
  assert.equal(calls[0].url, "http://gateway/api/inflight")
  assert.equal(calls[0].headers.Authorization, "Bearer sk-1", "the auth header is forwarded")
  assert.equal(
    calls[0].headers.Accept,
    "text/event-stream",
    "the SSE handshake header wins over the axios default",
  )
  assert.equal(client.isRunning(), false)
  globalThis.fetch = realFetch
}

// stop() aborts an open stream and leaves no pending reconnect behind
{
  let aborted = 0
  globalThis.fetch = (async (_input: any, init: any) => {
    const signal: AbortSignal | undefined = init?.signal
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        signal?.addEventListener("abort", () => {
          aborted += 1
          controller.error(new Error("aborted"))
        })
        controller.enqueue(new TextEncoder().encode(": ping\n\n"))
      },
    })
    return new Response(body, { status: 200 })
  }) as typeof fetch

  let opens = 0
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 5,
    onOpen: () => {
      opens += 1
    },
    onEvent: () => {},
  })
  client.start()
  await until(() => opens === 1, "the first stream to open")
  client.stop()
  await until(() => aborted === 1, "the abort to reach the stream")
  await new Promise((resolve) => setTimeout(resolve, 30))
  assert.equal(opens, 1, "a stopped client does not reconnect")
  globalThis.fetch = realFetch
}

// axios defaults carry `Accept: application/json`; the SSE handshake must win
{
  const headers = headersFromDefaults({
    Accept: "application/json, text/plain, */*",
    Authorization: "Bearer sk-1",
  })
  assert.ok(headers.Accept.startsWith("application/json"), "defaults are copied verbatim")
  const calls = stubFetch([streamResponse([])])
  let errors = 0
  const client = createSseClient({
    url: "/api/inflight",
    headers,
    baseDelayMs: 5,
    onEvent: () => {},
    onError: () => {
      errors += 1
    },
  })
  client.start()
  await until(() => errors >= 1, "the empty stream to end")
  client.stop()
  assert.equal(
    calls[0].headers.Accept,
    "text/event-stream",
    "the Accept header sent on the wire is the SSE one",
  )
  assert.equal(calls[0].headers.Authorization, "Bearer sk-1", "auth still travels with it")
  globalThis.fetch = realFetch
}

// stop() silences the loop: a queued chunk settling afterwards is not delivered
{
  globalThis.fetch = (async () =>
    streamResponse(['event: inflight\ndata: {"seq":1,"total":1,"routes":[]}\n\n'])) as typeof fetch
  const events: string[] = []
  let errors = 0
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 5,
    // The frame is already queued in the body when `onOpen` runs, so stopping
    // here is the "enqueue + stop() in one task" race: the pending `read()`
    // still settles, but nothing may be delivered from it.
    onOpen: () => client.stop(),
    onEvent: (event) => events.push(event.data),
    onError: () => {
      errors += 1
    },
  })
  client.start()
  await new Promise((resolve) => setTimeout(resolve, 40))
  assert.deepEqual(events, [], "no event reaches the caller after stop()")
  assert.equal(errors, 0, "the end-of-stream error is suppressed too")
  assert.equal(client.isRunning(), false)
  globalThis.fetch = realFetch
}

// a non-2xx response is an error (and still retried)
{
  const calls = stubFetch([
    new Response("nope", { status: 401 }),
    new Response("nope", { status: 401 }),
  ])
  const errors: unknown[] = []
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 5,
    onEvent: () => {},
    onError: (error) => errors.push(error),
  })
  client.start()
  await until(() => errors.length >= 2, "two failed attempts")
  client.stop()
  assert.ok(calls.length >= 2)
  assert.match(String(errors[0]), /401/, "the status is reported")
  assert.ok(
    errors.every((error) => error instanceof SseHttpError && error.status === 401),
    "the status is structured, not just formatted into the message",
  )
  globalThis.fetch = realFetch
}

// 401/403 mean the credentials are gone; every other status is transient
{
  assert.equal(isAuthFailure(new SseHttpError(401)), true)
  assert.equal(isAuthFailure(new SseHttpError(403)), true)
  assert.equal(isAuthFailure(new SseHttpError(500)), false)
  assert.equal(isAuthFailure(new Error("SSE request failed with status 401")), false)
  assert.equal(isAuthFailure(undefined), false)
}

// a 401 ends the loop instead of retrying — the inflight store's wiring
{
  const calls = stubFetch([
    new Response("nope", { status: 401 }),
    new Response("nope", { status: 401 }),
  ])
  const failures: unknown[] = []
  let reauths = 0
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 5,
    onEvent: () => {},
    onError: (error) => {
      failures.push(error)
      if (!isAuthFailure(error)) return
      // Exactly what `stores/inflight.ts` does: drop the stream, then hand the
      // failure to the shared `handle401()` and do not queue a reconnect.
      client.stop()
      reauths += 1
    },
  })
  client.start()
  await until(() => failures.length >= 1, "the 401 to be reported")
  await new Promise((resolve) => setTimeout(resolve, 40))
  assert.equal(reauths, 1, "the re-auth handler runs exactly once")
  assert.equal(failures.length, 1, "a 401 is not retried")
  assert.equal(calls.length, 1, "no second request is made")
  assert.equal(client.isRunning(), false)
  globalThis.fetch = realFetch
}

// ── accept header hardening ────────────────────────────────────────
// `fetch`'s `Headers` merges keys that differ only in case, so a stray `accept`
// copied from the axios defaults would otherwise be glued onto the handshake.
{
  const calls = stubFetch([streamResponse([])])
  let errors = 0
  const client = createSseClient({
    url: "/api/inflight",
    headers: {
      accept: "application/json",
      ACCEPT: "text/plain",
      "Content-Type": "application/json",
      Authorization: "Bearer sk-1",
    },
    baseDelayMs: 1000,
    onEvent: () => {},
    onError: () => {
      errors += 1
    },
  })
  client.start()
  await until(() => errors >= 1, "the empty stream to end")
  client.stop()

  const wire = new Headers(calls[0].headers)
  assert.equal(
    wire.get("accept"),
    "text/event-stream",
    "every case variant of Accept is dropped, so Headers cannot merge them",
  )
  assert.equal(wire.get("content-type"), null, "a bodyless GET forwards no Content-Type")
  assert.equal(wire.get("authorization"), "Bearer sk-1", "other headers still travel")
  globalThis.fetch = realFetch
}

// ── UTF-8 across chunk boundaries ──────────────────────────────────
// `TextDecoder` is used in streaming mode, so a multi-byte character split over
// two `read()` results must not turn into U+FFFD.
{
  // The name is built from code points because the i18n gate forbids CJK
  // literals outside `src/lib/i18n/`, and this test needs multi-byte characters.
  const model = String.fromCharCode(0x6a21, 0x578b)
  const frame = `event: inflight\ndata: {"model_name":"${model}-α"}\n\n`
  const bytes = new TextEncoder().encode(frame)
  const cjk = bytes.findIndex((byte) => byte >= 0x80)
  assert.ok(cjk > 0, "the frame really contains a multi-byte character")
  stubFetch([byteResponse([bytes.slice(0, cjk + 1), bytes.slice(cjk + 1)])])

  const events: string[] = []
  let errors = 0
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 1000,
    onEvent: (event) => events.push(event.data),
    onError: () => {
      errors += 1
    },
  })
  client.start()
  await until(() => errors >= 1, "the stream to end")
  client.stop()

  assert.deepEqual(events, [`{"model_name":"${model}-α"}`], "the split character is reassembled")
  globalThis.fetch = realFetch
}

// ── start() is single-flight ───────────────────────────────────────
{
  const calls = stubFetch([streamResponse([]), streamResponse([])])
  let errors = 0
  const client = createSseClient({
    url: "/api/inflight",
    // Long enough that the retry timer cannot fire while the assertions run.
    baseDelayMs: 1000,
    onEvent: () => {},
    onError: () => {
      errors += 1
    },
  })
  client.start()
  client.start()
  client.start()
  await until(() => errors >= 1, "the first stream to end")
  assert.equal(calls.length, 1, "three start() calls open exactly one stream")
  client.stop()
  globalThis.fetch = realFetch
}

// stop() then more chunks: the open body stays silent
{
  let push = (_chunk: string): void => {}
  globalThis.fetch = (async () => {
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        push = (chunk) => controller.enqueue(new TextEncoder().encode(chunk))
      },
    })
    return new Response(body, { status: 200 })
  }) as typeof fetch

  const events: string[] = []
  let opens = 0
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 1000,
    onOpen: () => {
      opens += 1
    },
    onEvent: (event) => events.push(event.data),
  })
  client.start()
  await until(() => opens === 1, "the stream to open")
  client.stop()
  push('event: inflight\ndata: {"seq":1}\n\n')
  await new Promise((resolve) => setTimeout(resolve, 30))
  assert.deepEqual(events, [], "a chunk enqueued after stop() reaches nobody")
  assert.equal(client.isRunning(), false)
  globalThis.fetch = realFetch
}

// an oversized frame drops the connection: reconnect, no delivery, no clearing
{
  const calls = stubFetch([
    streamResponse([
      'event: inflight\ndata: {"seq":1}\n\n',
      `data: ${"x".repeat(SSE_MAX_LINE_LENGTH)}\n\n`,
    ]),
    streamResponse([]),
  ])
  const events: string[] = []
  const errors: unknown[] = []
  // Replica of `stores/inflight.ts`: a frame replaces the state, any non-auth
  // error only flags it stale. The snapshot must survive the reconnect — a
  // broken stream is not proof that nothing is in flight.
  let state = emptyInflightState()
  const client = createSseClient({
    url: "/api/inflight",
    baseDelayMs: 5,
    onEvent: (event) => {
      events.push(event.data)
      const parsed = parseInflightSnapshot(event.data)
      if (parsed !== null) state = applySnapshot(state, parsed)
    },
    onError: (error) => {
      errors.push(error)
      state = markDisconnected(state)
    },
  })
  client.start()
  await until(() => calls.length >= 2, "the client to reconnect after the violation")
  client.stop()

  assert.ok(errors[0] instanceof SseFrameLimitError, "the cap surfaces as a stream error")
  assert.deepEqual(events, ['{"seq":1}'], "frames before the violation are still delivered")
  assert.deepEqual(state.snapshot, { seq: 1, total: 0, routes: [] }, "the snapshot is kept")
  assert.equal(state.stale, true, "and flagged stale instead of being cleared")
  assert.equal(client.isRunning(), false)
  globalThis.fetch = realFetch
}

console.log("# sse.test: all assertions passed")

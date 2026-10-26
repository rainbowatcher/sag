/**
 * Minimal Server-Sent Events client built on `fetch` + `ReadableStream`.
 *
 * `EventSource` cannot send request headers, so it cannot carry the gateway's
 * auth header (the one every `/api/*` call uses). This module reads the SSE
 * byte stream itself instead: `createSseParser` turns decoded text chunks into
 * event frames (cross-chunk safe, `: ping` heartbeat comments ignored) and
 * `createSseClient` owns the connection loop — open, consume, and reconnect
 * with exponential backoff until `stop()`.
 */

/** One dispatched SSE frame. `event` defaults to `"message"` per the spec. */
export interface SseEvent {
  event: string
  /** `data:` lines joined with a newline; never empty (a data-less frame is dropped). */
  data: string
  /** Last `id:` seen on the stream, if any. */
  id: string | null
}

export interface SseParser {
  /** Feed one decoded chunk; returns the frames completed by it. */
  push(chunk: string): SseEvent[]
  /** End of stream: flush a trailing frame that has no closing blank line. */
  flush(): SseEvent[]
}

/** Locate the next line break, or `null` when the line is still incomplete. */
function findLineEnd(text: string): { index: number; length: number } | null {
  for (let i = 0; i < text.length; i++) {
    const char = text[i]
    if (char === "\n") return { index: i, length: 1 }
    if (char === "\r") {
      // A trailing carriage return may be the first half of a CRLF split across
      // chunks, so wait for the next chunk before deciding.
      if (i + 1 >= text.length) return null
      return { index: i, length: text[i + 1] === "\n" ? 2 : 1 }
    }
  }
  return null
}

/**
 * Incremental SSE frame parser (spec §9.2.6, minus what the gateway does not
 * use: `retry` is ignored because the client owns the backoff schedule).
 */
export function createSseParser(): SseParser {
  let buffer = ""
  let eventName = ""
  let lastId: string | null = null
  let dataLines: string[] = []

  function resetFrame(): void {
    eventName = ""
    dataLines = []
  }

  function dispatch(out: SseEvent[]): void {
    const data = dataLines.join("\n")
    if (data !== "") out.push({ event: eventName || "message", data, id: lastId })
    resetFrame()
  }

  function handleLine(line: string, out: SseEvent[]): void {
    if (line === "") {
      dispatch(out)
      return
    }
    // A line starting with a colon is a comment — the gateway's `: ping` heartbeat.
    if (line.startsWith(":")) return
    const colon = line.indexOf(":")
    const field = colon === -1 ? line : line.slice(0, colon)
    let value = colon === -1 ? "" : line.slice(colon + 1)
    if (value.startsWith(" ")) value = value.slice(1)
    switch (field) {
      case "event":
        eventName = value
        break
      case "data":
        dataLines.push(value)
        break
      case "id":
        if (!value.includes("\u0000")) lastId = value
        break
      default:
        // `retry` and unknown fields are ignored on purpose.
        break
    }
  }

  return {
    push(chunk: string): SseEvent[] {
      const out: SseEvent[] = []
      buffer += chunk
      for (let end = findLineEnd(buffer); end !== null; end = findLineEnd(buffer)) {
        const line = buffer.slice(0, end.index)
        buffer = buffer.slice(end.index + end.length)
        handleLine(line, out)
      }
      return out
    },
    flush(): SseEvent[] {
      const out: SseEvent[] = []
      // End of stream: a trailing lone CR can no longer be the first half of a
      // CRLF, so drop it instead of leaving it inside the last field value (on
      // an `event:` line it would silently rename the frame).
      if (buffer.endsWith("\r")) buffer = buffer.slice(0, -1)
      if (buffer !== "") {
        const line = buffer
        buffer = ""
        handleLine(line, out)
      }
      dispatch(out)
      return out
    },
  }
}

/**
 * Delay before the n-th consecutive retry: `baseMs * factor ** n`, clamped to
 * `maxMs`. `attempt` 0 is the first retry after a drop.
 */
export function nextRetryDelay(attempt: number, baseMs = 1000, maxMs = 30000, factor = 2): number {
  const step = Math.max(0, Math.floor(attempt))
  const raw = baseMs * Math.pow(factor, step)
  return Math.min(Math.max(0, maxMs), Math.max(0, Math.round(raw)))
}

/**
 * Copy the string-valued headers out of an axios `defaults.headers.common` map,
 * so an SSE request carries exactly the auth header every `/api/*` call uses.
 */
export function headersFromDefaults(
  defaults: Record<string, unknown> | undefined | null,
): Record<string, string> {
  const headers: Record<string, string> = {}
  if (!defaults) return headers
  for (const [name, value] of Object.entries(defaults)) {
    if (typeof value === "string" && value !== "") headers[name] = value
  }
  return headers
}

/** Callbacks and knobs of `createSseClient`. */
export interface SseClientOptions {
  /** Absolute or root-relative stream URL, resolved against the page origin. */
  url: string
  /** Request headers, typically `headersFromDefaults(api.defaults.headers.common)`. */
  headers?: Record<string, string>
  /** Called once the response body is open (before any event arrives). */
  onOpen?: () => void
  /** Called for every dispatched frame. */
  onEvent: (event: SseEvent) => void
  /** Called when the connection drops or the request fails. */
  onError?: (error: unknown) => void
  /** First retry delay (default 1000 ms). */
  baseDelayMs?: number
  /** Delay ceiling (default 30000 ms). */
  maxDelayMs?: number
}

export interface SseClient {
  /** Start the connection loop; a no-op when already running. */
  start(): void
  /** Close the stream and cancel any pending reconnect. */
  stop(): void
  /** Whether the loop is currently running. */
  isRunning(): boolean
}

/**
 * A non-2xx answer to the stream request. The status stays structured (instead
 * of only living in the message) so a caller can tell "the credentials are
 * gone" from a transient failure.
 */
export class SseHttpError extends Error {
  readonly status: number

  constructor(status: number) {
    super(`SSE request failed with status ${status}`)
    this.name = "SseHttpError"
    this.status = status
  }
}

/**
 * Whether a stream failure means the credentials are gone (`401`/`403`).
 *
 * Every other `/api/*` channel routes those to `useConnectionStore().handle401()`
 * instead of retrying; the SSE stream must not be the one exception, or a
 * rotated key leaves the page retrying forever behind a "reconnecting" label.
 */
export function isAuthFailure(error: unknown): boolean {
  return error instanceof SseHttpError && (error.status === 401 || error.status === 403)
}

/**
 * Long-lived SSE connection with automatic exponential-backoff reconnect.
 *
 * The loop is single-flight: `start()` while running is a no-op, and `stop()`
 * aborts the in-flight request plus the pending retry timer, so a page unmount
 * cannot leave a zombie stream behind.
 */
export function createSseClient(options: SseClientOptions): SseClient {
  const baseDelayMs = options.baseDelayMs ?? 1000
  const maxDelayMs = options.maxDelayMs ?? 30000

  let running = false
  let attempt = 0
  let controller: AbortController | null = null
  let retryTimer: ReturnType<typeof setTimeout> | null = null

  function scheduleRetry(): void {
    if (!running) return
    const delay = nextRetryDelay(attempt, baseDelayMs, maxDelayMs)
    attempt += 1
    retryTimer = setTimeout(() => {
      retryTimer = null
      void connect()
    }, delay)
  }

  async function consume(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader()
    const decoder = new TextDecoder()
    const parser = createSseParser()
    /**
     * Dispatch only while the loop is live. A chunk whose `read()` settles after
     * `stop()` would otherwise reach the caller and repaint a closed panel as
     * live (clearing its `stale` flag).
     */
    const emit = (events: SseEvent[]): void => {
      if (!running) return
      for (const event of events) options.onEvent(event)
    }
    try {
      for (;;) {
        // Stream reads must stay sequential: each chunk continues the previous one.
        // oxlint-disable-next-line no-await-in-loop
        const { value, done } = await reader.read()
        if (done) break
        if (!running) return
        if (value) emit(parser.push(decoder.decode(value, { stream: true })))
      }
      const tail = decoder.decode()
      emit(tail ? parser.push(tail) : [])
      emit(parser.flush())
    } finally {
      reader.releaseLock()
    }
  }

  async function connect(): Promise<void> {
    if (!running) return
    controller = new AbortController()
    try {
      const response = await fetch(options.url, {
        method: "GET",
        // The SSE handshake header goes last: the caller's headers come from
        // `api.defaults.headers.common`, which carries `Accept: application/json`.
        headers: { ...options.headers, Accept: "text/event-stream" },
        signal: controller.signal,
        cache: "no-store",
      })
      if (!running) return
      if (!response.ok || !response.body) {
        throw new SseHttpError(response.status)
      }
      // Any successful connection resets the backoff, so a flaky network does
      // not keep growing the delay forever.
      attempt = 0
      options.onOpen?.()
      await consume(response.body)
      if (!running) return
      options.onError?.(new Error("SSE stream ended"))
    } catch (error) {
      if (!running) return
      options.onError?.(error)
    } finally {
      controller = null
    }
    scheduleRetry()
  }

  return {
    start(): void {
      if (running) return
      running = true
      attempt = 0
      void connect()
    },
    stop(): void {
      running = false
      if (retryTimer !== null) {
        clearTimeout(retryTimer)
        retryTimer = null
      }
      controller?.abort()
      controller = null
    },
    isRunning(): boolean {
      return running
    },
  }
}

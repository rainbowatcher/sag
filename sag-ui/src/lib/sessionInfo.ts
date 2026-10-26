/**
 * Display helpers for the session info card on the logs page.
 *
 * `GET /api/logs?session=<key>` returns a `SessionInfo` registry row whose
 * fields come straight from the backend: `session_source` is one of the
 * extraction-chain steps (`meta` / `top` / `pc` / `fb`), the timestamps are
 * ISO-8601, and the first-message preview is already whitespace-collapsed and
 * capped at 160 characters. These helpers turn those raw values into the
 * single-line strings the card renders, so the formatting is unit-testable
 * without mounting a component.
 */

/** Session sources emitted by the backend extraction chain. */
export const SESSION_SOURCE_KINDS = ["meta", "top", "pc", "fb"] as const

/** One `SessionInfo.session_source` value. */
export type SessionSourceKind = (typeof SESSION_SOURCE_KINDS)[number]

/** Placeholder rendered for a registry field that is absent. */
export const SESSION_PLACEHOLDER = "—"

/**
 * Narrow a raw `session_source` to a known kind, or `null` for anything the UI
 * has no label for (the card then falls back to the raw value).
 */
export function sessionSourceKind(source: string | null | undefined): SessionSourceKind | null {
  const value = (source ?? "").trim().toLowerCase()
  return SESSION_SOURCE_KINDS.find((kind) => kind === value) ?? null
}

/** Single-line display text for an optional registry field. */
export function sessionText(value: string | null | undefined): string {
  const text = (value ?? "").trim()
  return text === "" ? SESSION_PLACEHOLDER : text
}

/**
 * Single-line preview text: runs of whitespace collapse to one space (the
 * preview may still carry line breaks from the original message) and anything
 * longer than `max` characters is cut with a trailing `…`.
 */
export function sessionPreviewText(preview: string | null | undefined, max = 160): string {
  const text = (preview ?? "").replace(/\s+/g, " ").trim()
  if (text === "") return SESSION_PLACEHOLDER
  const limit = Math.max(1, Math.trunc(max))
  return text.length <= limit ? text : `${text.slice(0, limit)}…`
}

/** Format an ISO-8601 registry timestamp the way the logs table renders times. */
export function formatSessionTime(iso: string | null | undefined): string {
  const raw = (iso ?? "").trim()
  if (raw === "" || Number.isNaN(Date.parse(raw))) return SESSION_PLACEHOLDER
  return new Date(raw).toLocaleString("en-CA", { hour12: false }).replace(",", "")
}

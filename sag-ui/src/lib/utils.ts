export { cn } from "cnfast"

export function getErrorMessage(e: unknown, fallback = "Unknown error"): string {
  return (e as any)?.response?.data?.error?.message ?? (e as Error)?.message ?? fallback
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`
  const minutes = Math.floor(ms / 60_000)
  const seconds = Math.round((ms % 60_000) / 1000)
  if (minutes < 60) return `${minutes}m ${seconds}s`
  const hours = Math.floor(minutes / 60)
  const mins = minutes % 60
  return `${hours}h ${mins}m ${seconds}s`
}

export function formatCompactNumber(n: number): string {
  if (n < 1_000) return n.toLocaleString()
  if (n < 1_000_000) return `${fmtCompact(n / 1_000)}K`
  if (n < 1_000_000_000) return `${fmtCompact(n / 1_000_000)}M`
  return `${fmtCompact(n / 1_000_000_000)}B`
}

/** Format to one decimal place, stripping trailing ".0". */
function fmtCompact(v: number): string {
  const s = v.toFixed(1)
  return s.endsWith(".0") ? s.slice(0, -2) : s
}

/**
 * Map a success rate percentage (0–100) to a colour via OKLCH hue sweep.
 *
 * Lightness and chroma are held constant so the ramp is perceptually
 * uniform: green (100 %) → yellow → orange → red (0 %).
 */
export function successRateColor(rate: number): string {
  const clamped = Math.max(0, Math.min(100, rate))
  const t = 1 - clamped / 100
  const h = 142 - t * (142 - 25)
  return `oklch(0.52 0.2 ${h.toFixed(1)})`
}

/**
 * Validate a JSON string intended for target `extra` fields.
 * Returns an error message if non-empty but not a valid JSON object, else null.
 */
export function validateExtraJson(extra: string): string | null {
  const trimmed = extra.trim()
  if (!trimmed) return null
  try {
    const parsed = JSON.parse(trimmed)
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return "Expected a JSON object"
    }
    return null
  } catch (e: any) {
    return e?.message ? `Invalid JSON: ${e.message}` : "Invalid JSON"
  }
}

/**
 * Format a static remaining-time label from a UNIX-seconds timestamp.
 *
 * `epochSecs` is in **seconds**; `Date.now()` is in **milliseconds**, so the
 * delta is `epochSecs * 1000 - Date.now()`.
 *
 * @param epochSecs  UNIX timestamp in seconds.
 * @param expiredLabel  Text to show when the moment has already elapsed.
 *   Defaults to "已可恢复" (for cooldown). Pass "已可探测" for probe times.
 * @returns Human-readable remaining time like "3m 45s", or `expiredLabel`.
 */
export function formatRemaining(epochSecs: number, expiredLabel = "已可恢复"): string {
  const ms = epochSecs * 1000 - Date.now()
  if (ms <= 0) return expiredLabel
  const totalSec = Math.floor(ms / 1000)
  const hours = Math.floor(totalSec / 3600)
  const minutes = Math.floor((totalSec % 3600) / 60)
  const seconds = totalSec % 60
  if (hours > 0) return `${hours}h ${minutes}m`
  if (minutes > 0) return `${minutes}m ${seconds}s`
  return `${seconds}s`
}

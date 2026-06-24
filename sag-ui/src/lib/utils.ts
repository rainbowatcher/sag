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

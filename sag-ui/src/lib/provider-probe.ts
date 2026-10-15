import type { ApiKind } from "@/types/api"

/** Fixed probe preference order over protocol sections (never `image`). */
export const PROBE_KIND_ORDER: ApiKind[] = ["chat_completions", "responses", "anthropic_messages"]

/**
 * Default section for model listing / model tests: the provider's `fallback`
 * protocol when declared, else a fixed preference order (chat_completions
 * first). Never returns the `image` section — independent of key insertion
 * order in the `apis` map.
 */
export function defaultProbeKind(
  apis: Partial<Record<ApiKind, unknown>>,
  fallback?: ApiKind | null,
): ApiKind | null {
  const keys = Object.keys(apis) as ApiKind[]
  if (fallback && fallback !== "image" && keys.includes(fallback)) return fallback
  for (const kind of PROBE_KIND_ORDER) {
    if (keys.includes(kind)) return kind
  }
  return keys.find((k) => k !== "image") ?? null
}

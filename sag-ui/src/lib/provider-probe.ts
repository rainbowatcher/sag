import type { ApiKind, ProtocolApiKind } from "@/types/api"

/**
 * Fixed probe preference order over protocol sections (never `image`).
 * `systemone` and `image` are deliberately excluded: the backend has no
 * `minimal_request` for either, so probing them only returns
 * `verdict: "error"`.
 */
export const PROBE_KIND_ORDER: ProtocolApiKind[] = [
  "chat_completions",
  "responses",
  "anthropic_messages",
]

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
  // Runtime guard kept on purpose: `fallback` is stored as a plain `ApiKind`
  // (the editor/composable allow any catalogue key), so `image` / `systemone`
  // can still arrive here and must never become the probe section.
  if (fallback && fallback !== "image" && fallback !== "systemone" && keys.includes(fallback))
    return fallback
  for (const kind of PROBE_KIND_ORDER) {
    if (keys.includes(kind)) return kind
  }
  return keys.find((k) => k !== "image" && k !== "systemone") ?? null
}

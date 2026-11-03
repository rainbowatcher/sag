/**
 * `providers[].apis` 目录 ↔ `ApisEditor` 草稿的纯映射层。
 * `PUT /api/providers` 对 `apis` 是整目录替换语义，所以回写必须保留不在受管
 * `order` 范围内的 key（例如后端已支持的 `systemone`），同时让被删除的受管 key
 * 真正消失 —— 这两种需求都不能靠一次 `{ ...base, ...managed }` 浅合并满足。
 */
import type { ApiConfig, ApiKind, ChatIrApiKind } from "@/types/api"

/** One key/value row of a draft's map fields (`headers`, `role_mappings`, …). */
export interface ApiDraftKvPair {
  key: string
  value: string
}

/**
 * Editable state of one `apis.<kind>` section: `kind` plus the per-kind config
 * fields, in the same string/pair shape the form controls bind to (numbers are
 * kept as raw input text, JSON maps as raw JSON text).
 */
export interface ApiDraft {
  /** Editor-local list identity (never persisted, never part of the value). */
  uid: number
  kind: ApiKind
  base_url: string
  models_endpoint: string
  headers: ApiDraftKvPair[]
  body_overrides: ApiDraftKvPair[]
  strip_fields: string[]
  model_overrides: ApiDraftKvPair[]
  role_mappings: ApiDraftKvPair[]
  /** Tri-state of `ApiConfig.anthropic_cache_control`: unset / `true` / `false`. */
  anthropic_cache_control: "inherit" | "enable" | "disable"
  /** Raw text; empty string = unset. */
  timeout_secs: string
  /** Raw text; empty string = unset. */
  chunk_timeout_secs: string
  /** Raw text; empty string = unset. */
  ttfb_timeout_secs: string
  /** Editor-only accordion state (never persisted). */
  advancedOpen: boolean
}

// `uid` is a list key, not part of the value: a monotonic counter keeps every
// draft distinct across repeated `catalogToDrafts` calls in one page session.
let uidCounter = 0

function pairsFrom(obj: Record<string, string> | undefined): ApiDraftKvPair[] {
  return Object.entries(obj ?? {}).map(([key, value]) => ({ key, value }))
}

function jsonPairsFrom(obj: Record<string, Record<string, any>> | undefined): ApiDraftKvPair[] {
  return Object.entries(obj ?? {}).map(([key, value]) => ({ key, value: JSON.stringify(value) }))
}

function configToDraft(kind: ApiKind, cfg?: ApiConfig): ApiDraft {
  return {
    uid: ++uidCounter,
    kind,
    base_url: cfg?.base_url ?? "",
    models_endpoint: cfg?.models_endpoint ?? "",
    headers: pairsFrom(cfg?.headers),
    body_overrides: Object.entries(cfg?.body_overrides ?? {}).map(([key, value]) => ({
      key,
      value: typeof value === "string" ? value : JSON.stringify(value),
    })),
    strip_fields: [...(cfg?.strip_fields ?? [])],
    model_overrides: jsonPairsFrom(cfg?.model_overrides),
    role_mappings: pairsFrom(cfg?.role_mappings),
    anthropic_cache_control:
      cfg?.anthropic_cache_control === undefined
        ? "inherit"
        : cfg.anthropic_cache_control
          ? "enable"
          : "disable",
    timeout_secs: cfg?.timeout_secs !== undefined ? String(cfg.timeout_secs) : "",
    chunk_timeout_secs: cfg?.chunk_timeout_secs !== undefined ? String(cfg.chunk_timeout_secs) : "",
    ttfb_timeout_secs: cfg?.ttfb_timeout_secs !== undefined ? String(cfg.ttfb_timeout_secs) : "",
    advancedOpen: false,
  }
}

/**
 * Narrow a stored fallback kind to a chat-IR protocol; `image` / `systemone`
 * cannot be a fallback (no chat IR, the backend rejects them). The provider
 * probe helper types `fallback` as `ApiKind | null` because it also serves
 * catalogues that carry `image`.
 */
export function toChatIrFallback(kind: ApiKind | null): ChatIrApiKind | null {
  return kind === "chat_completions" || kind === "responses" || kind === "anthropic_messages"
    ? kind
    : null
}

export function kvToObject(pairs: readonly ApiDraftKvPair[]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const { key, value } of pairs) {
    if (key.trim() && value.trim()) out[key.trim()] = value.trim()
  }
  return out
}

function kvToJson(pairs: ApiDraftKvPair[]): Record<string, any> {
  const out: Record<string, any> = {}
  for (const { key, value } of pairs) {
    if (!key.trim() || !value.trim()) continue
    try {
      out[key.trim()] = JSON.parse(value.trim())
    } catch {
      out[key.trim()] = value.trim()
    }
  }
  return out
}

function parseOptionalNumber(raw: string): number | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  const n = Number(trimmed)
  return Number.isFinite(n) && n >= 0 ? n : undefined
}

function draftToConfig(d: ApiDraft): ApiConfig {
  const cfg: ApiConfig = { base_url: d.base_url.trim() }
  const modelsEndpoint = d.models_endpoint.trim()
  if (modelsEndpoint) cfg.models_endpoint = modelsEndpoint
  const headers = kvToObject(d.headers)
  if (Object.keys(headers).length > 0) cfg.headers = headers
  const bodyOverrides = kvToJson(d.body_overrides)
  if (Object.keys(bodyOverrides).length > 0) cfg.body_overrides = bodyOverrides
  const stripFields = d.strip_fields.filter((f) => f.trim()).map((f) => f.trim())
  if (stripFields.length > 0) cfg.strip_fields = stripFields
  const modelOverrides: Record<string, Record<string, any>> = {}
  for (const { key, value } of d.model_overrides) {
    if (!key.trim() || !value.trim()) continue
    try {
      const parsed = JSON.parse(value.trim())
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
        modelOverrides[key.trim()] = parsed
      }
    } catch {
      // Dropped here; the editor surfaces the JSON error before submit.
    }
  }
  if (Object.keys(modelOverrides).length > 0) cfg.model_overrides = modelOverrides
  const roleMappings = kvToObject(d.role_mappings)
  if (Object.keys(roleMappings).length > 0) cfg.role_mappings = roleMappings
  if (d.anthropic_cache_control !== "inherit")
    cfg.anthropic_cache_control = d.anthropic_cache_control === "enable"
  const timeout = parseOptionalNumber(d.timeout_secs)
  if (timeout !== undefined) cfg.timeout_secs = timeout
  const chunkTimeout = parseOptionalNumber(d.chunk_timeout_secs)
  if (chunkTimeout !== undefined) cfg.chunk_timeout_secs = chunkTimeout
  const ttfbTimeout = parseOptionalNumber(d.ttfb_timeout_secs)
  if (ttfbTimeout !== undefined) cfg.ttfb_timeout_secs = ttfbTimeout
  return cfg
}

/**
 * Sections to render: only the kinds listed in `order` that `apis` actually
 * declares, in `order` sequence. Keys outside `order` never become a section
 * they cannot edit — `draftsToCatalog` passes them through untouched instead.
 */
export function catalogToDrafts(
  apis: Partial<Record<ApiKind, ApiConfig>>,
  order: readonly ApiKind[],
): ApiDraft[] {
  const drafts: ApiDraft[] = []
  for (const kind of order) {
    const cfg = apis[kind]
    if (cfg) drafts.push(configToDraft(kind, cfg))
  }
  return drafts
}

/**
 * Faithful whole-directory write-back for `PUT /api/providers`.
 *
 * - Keys **outside** `order` are passed through from `base` verbatim.
 * - Keys **inside** `order` are owned by `drafts`: present → the draft's value,
 *   absent → dropped (removing a section really removes it, `base` cannot
 *   resurrect it).
 *
 * Key order: the `order` kinds that have a draft, then the pass-through keys in
 * `base` order. Drafts whose kind is not in `order` are ignored.
 */
export function draftsToCatalog(
  drafts: readonly ApiDraft[],
  base: Partial<Record<ApiKind, ApiConfig>>,
  order: readonly ApiKind[],
): Record<ApiKind, ApiConfig> {
  const byKind = new Map<ApiKind, ApiDraft>()
  for (const draft of drafts) byKind.set(draft.kind, draft)

  const out: Partial<Record<ApiKind, ApiConfig>> = {}
  for (const kind of order) {
    const draft = byKind.get(kind)
    if (draft) out[kind] = draftToConfig(draft)
  }

  const managed = new Set<ApiKind>(order)
  for (const [kind, cfg] of Object.entries(base) as [ApiKind, ApiConfig | undefined][]) {
    if (cfg === undefined || managed.has(kind)) continue
    out[kind] = cfg
  }
  return out as Record<ApiKind, ApiConfig>
}

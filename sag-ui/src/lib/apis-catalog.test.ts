import assert from "node:assert/strict"
import type { ApiConfig, ApiKind } from "../types/api.ts"
import type { ApiDraft } from "./apis-catalog.ts"
import { catalogToDrafts, draftsToCatalog } from "./apis-catalog.ts"

// Boundary tests for the `apis` catalog write-back.
// `PUT /api/providers` replaces the whole `apis` directory, so a save must not
// drop keys the editor cannot render (`systemone`, …) — yet it must still drop
// the managed keys whose section the user removed. These tests pin both halves
// of that contract, plus the key order the payload is built in.

const ORDER: readonly ApiKind[] = ["chat_completions", "responses", "anthropic_messages", "image"]

function cfg(partial: Partial<ApiConfig> = {}): ApiConfig {
  return { base_url: "https://api.example.com/v1", ...partial }
}

function draft(kind: ApiKind, partial: Partial<Omit<ApiDraft, "kind">> = {}): ApiDraft {
  return {
    uid: 0,
    kind,
    base_url: "https://api.example.com/v1",
    models_endpoint: "",
    headers: [],
    body_overrides: [],
    strip_fields: [],
    model_overrides: [],
    role_mappings: [],
    anthropic_cache_control: "inherit",
    timeout_secs: "",
    chunk_timeout_secs: "",
    ttfb_timeout_secs: "",
    advancedOpen: false,
    ...partial,
  }
}

// ── pass-through: keys outside `order` survive a save verbatim ──────
{
  const base = {
    chat_completions: cfg({ base_url: "https://chat.example.com/v1" }),
    systemone: cfg({
      base_url: "https://decision.example.com",
      headers: { "x-decision": "on" },
      timeout_secs: 30,
    }),
  }
  const out = draftsToCatalog(
    [draft("chat_completions", { base_url: "https://chat.example.com/v1" })],
    base,
    ORDER,
  )

  assert.equal("systemone" in out, true, "systemone is not in order → must be kept")
  assert.deepEqual(out.systemone, base.systemone, "pass-through value is unchanged, field by field")
  assert.deepEqual(Object.keys(out), ["chat_completions", "systemone"])
}

// ── removal: a managed key dropped from drafts must not come back ───
{
  const base = {
    chat_completions: cfg({ base_url: "https://chat.example.com/v1" }),
    systemone: cfg({ base_url: "https://decision.example.com" }),
  }
  const out = draftsToCatalog([], base, ORDER)

  assert.equal(
    "chat_completions" in out,
    false,
    "managed key with no draft is deleted, not revived",
  )
  assert.equal("systemone" in out, true, "pass-through key is untouched by an empty draft list")
  assert.deepEqual(Object.keys(out), ["systemone"])

  // Removing one of two managed sections keeps only the surviving one.
  const kept = draftsToCatalog([draft("responses")], base, ORDER)
  assert.equal("chat_completions" in kept, false)
  assert.equal("responses" in kept, true)
}

// ── round-trip: catalogToDrafts → draftsToCatalog is lossless ───────
{
  const order: readonly ApiKind[] = ["chat_completions", "systemone", "image"]
  const original: Partial<Record<ApiKind, ApiConfig>> = {
    chat_completions: cfg({
      base_url: "https://chat.example.com/v1",
      models_endpoint: "v1/models",
      headers: { authorization: "Bearer key" },
      body_overrides: { max_tokens: 4096, temperature: 0.2 },
      strip_fields: ["field_a", "field_b"],
      model_overrides: { "gpt-4o": { max_tokens: 8192 } },
      role_mappings: { developer: "system" },
      anthropic_cache_control: true,
      timeout_secs: 60,
      chunk_timeout_secs: 15,
      ttfb_timeout_secs: 5,
    }),
    systemone: cfg({
      base_url: "https://decision.example.com",
      anthropic_cache_control: false,
    }),
  }

  const drafts = catalogToDrafts(original, order)
  assert.deepEqual(
    drafts.map((d) => d.kind),
    ["chat_completions", "systemone"],
    "catalogToDrafts follows `order`, not the catalog's own key order",
  )
  assert.deepEqual(draftsToCatalog(drafts, original, order), original)
}

// ── key order: `order` first, then the pass-through keys ────────────
{
  // Drafts and base are both deliberately unsorted: only `order` decides.
  const out = draftsToCatalog(
    [draft("systemone"), draft("chat_completions")],
    { systemone: cfg(), chat_completions: cfg() },
    ["chat_completions", "systemone"],
  )
  assert.deepEqual(Object.keys(out), ["chat_completions", "systemone"])

  // Pass-through keys follow the managed ones, in `base` order.
  const mixed = draftsToCatalog(
    [draft("chat_completions")],
    { systemone: cfg(), chat_completions: cfg(), image: cfg() },
    ["chat_completions"],
  )
  assert.deepEqual(Object.keys(mixed), ["chat_completions", "systemone", "image"])
}

// ── empty inputs ────────────────────────────────────────────────────
{
  assert.deepEqual(catalogToDrafts({}, ORDER), [], "no catalog → no drafts")
  assert.deepEqual(catalogToDrafts({}, []), [])
  assert.deepEqual(draftsToCatalog([], {}, ORDER), {}, "no drafts and no base → empty directory")
  assert.deepEqual(
    draftsToCatalog([], { systemone: cfg() }, ORDER),
    { systemone: cfg() },
    "an empty draft list still returns the pass-through keys",
  )
}

console.log("apis-catalog.test: all assertions passed")

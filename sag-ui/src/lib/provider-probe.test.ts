import assert from "node:assert/strict"
import type { ApiConfig, ApiKind, ChatIrApiKind } from "../types/api.ts"
import { PROBE_KIND_ORDER, defaultProbeKind } from "./provider-probe.ts"

// Boundary tests for the models-page probe target selection.
// `systemone` is a real `ApiKind` that can be declared in `apis`, but it is
// neither probeable (the backend has no `minimal_request` for it) nor a valid
// `fallback`, so it must never become the selected probe section.

function api(partial: Partial<ApiConfig> = {}): ApiConfig {
  return { base_url: "https://api.example.com", ...partial }
}

// ── PROBE_KIND_ORDER: protocols only, never systemone / image ───────
{
  // The constant is typed `ProtocolApiKind[]`; widen it to `ApiKind[]` here so
  // this negative boundary test can name the excluded kinds without a lying cast.
  const probeKinds: readonly ApiKind[] = PROBE_KIND_ORDER
  assert.equal(
    probeKinds.includes("systemone"),
    false,
    "systemone is not probeable (no backend minimal_request)",
  )
  assert.equal(probeKinds.includes("image"), false, "image is not a probe protocol")
  for (const kind of probeKinds) {
    assert.equal(
      kind === "systemone" || kind === "image",
      false,
      `PROBE_KIND_ORDER must not contain ${kind}`,
    )
  }
}

// ── defaultProbeKind: systemone is never part of the fallback ───────
{
  assert.equal(
    defaultProbeKind({ systemone: api() }, null),
    null,
    "a systemone-only provider has no probeable section",
  )
  assert.equal(
    defaultProbeKind({ image: api(), systemone: api() }, null),
    null,
    "image + systemone still yields no probeable section",
  )
}

// ── defaultProbeKind: real protocols win over systemone ─────────────
{
  assert.equal(
    defaultProbeKind({ chat_completions: api(), systemone: api() }, null),
    "chat_completions",
    "a declared protocol key beats the systemone fallback",
  )
}

// ── defaultProbeKind: explicit fallback still takes precedence ──────
{
  assert.equal(
    defaultProbeKind({ responses: api(), chat_completions: api() }, "responses"),
    "responses",
    "the declared fallback wins over the preference order",
  )
}

// ── defaultProbeKind: systemone is never accepted even when forced at runtime ──
{
  // 类型层期望的是 chat-IR 协议，这里故意绕过类型断言，验证运行时守卫仍在。
  const forced = "systemone" as unknown as ChatIrApiKind
  assert.equal(defaultProbeKind({ systemone: api() }, forced), null)
}

console.log("providerProbe.test: all assertions passed")

import assert from "node:assert/strict"
import type { Pipeline, Strategy } from "../types/api.ts"
import {
  PIPELINES,
  createEndpointField,
  isPipeline,
  pipelineSwitchReset,
  showsApiEndpoint,
  supportsSessionSticky,
  updateEndpointField,
} from "./routePipeline.ts"

// The route `pipeline` has three mutually exclusive field sets: `image` owns
// `api_endpoint`, `systemone` allows neither an endpoint nor session pinning,
// `chat` allows neither of them either. These tests pin the switch/payload rules
// that the add & edit forms share, so the two forms cannot drift apart.

const STRATEGIES: readonly Strategy[] = [
  "sticky_failover",
  "fill_first",
  "round_robin",
  "session_sticky",
]

// ── pipeline value set ──────────────────────────────────────────────

{
  // Compile-time: `systemone` is a legal `Pipeline` (serde name, no separator).
  const systemone: Pipeline = "systemone"
  assert.equal(systemone, "systemone")

  assert.deepEqual([...PIPELINES], ["chat", "image", "systemone"])
  for (const pipeline of PIPELINES) assert.equal(isPipeline(pipeline), true)
  assert.equal(isPipeline("system_one"), false)
  assert.equal(isPipeline("system-one"), false)
  assert.equal(isPipeline(""), false)
  assert.equal(isPipeline(null), false)
  assert.equal(isPipeline(2), false)
}

// ── api_endpoint visibility ─────────────────────────────────────────

{
  assert.equal(showsApiEndpoint("image"), true)
  assert.equal(showsApiEndpoint("chat"), false)
  assert.equal(showsApiEndpoint("systemone"), false)
}

// ── session_sticky support ──────────────────────────────────────────

{
  assert.equal(supportsSessionSticky("chat"), true)
  assert.equal(supportsSessionSticky("image"), true) // rejected by the strategy validation
  assert.equal(supportsSessionSticky("systemone"), false)
}

// ── pipeline switch: field clearing ─────────────────────────────────

{
  // image → systemone: the endpoint is no longer legal, so it is emptied.
  const reset = pipelineSwitchReset("systemone", "sticky_failover")
  assert.equal(reset.api_endpoint, "")

  // image → chat: same rule, an endpoint cannot survive outside image.
  assert.equal(pipelineSwitchReset("chat", "fill_first").api_endpoint, "")

  // chat → image: the endpoint belongs to image, keep whatever the user types.
  assert.equal(pipelineSwitchReset("image", "round_robin").api_endpoint, undefined)
}

{
  // Any pipeline → systemone clears both session pin fields.
  for (const from of PIPELINES) {
    for (const strategy of STRATEGIES) {
      const reset = pipelineSwitchReset("systemone", strategy)
      assert.equal(reset.session_ttl_secs, "", `${from} → systemone ttl`)
      assert.equal(reset.session_max_entries, "", `${from} → systemone capacity`)
    }
  }
}

{
  // A session_sticky selection is forced back to a strategy systemone supports.
  assert.equal(pipelineSwitchReset("systemone", "session_sticky").strategy, "sticky_failover")
  // Other strategies keep their value: systemone supports all four of them.
  assert.equal(pipelineSwitchReset("systemone", "round_robin").strategy, undefined)

  // image keeps the session fields: image+session_sticky is a validation error,
  // not something the switch silently rewrites.
  const toImage = pipelineSwitchReset("image", "session_sticky")
  assert.equal(toImage.session_ttl_secs, undefined)
  assert.equal(toImage.session_max_entries, undefined)
  assert.equal(toImage.strategy, undefined)
}

// ── create payload: omit vs send ────────────────────────────────────

{
  // systemone / chat omit the key entirely (not an empty string).
  assert.deepEqual(createEndpointField("systemone", "/dropped"), {})
  assert.deepEqual(createEndpointField("chat", "/dropped"), {})
  assert.equal("api_endpoint" in createEndpointField("systemone", "/dropped"), false)

  assert.deepEqual(createEndpointField("image", "  /images/generations  "), {
    api_endpoint: "/images/generations",
  })
}

// ── update payload: value vs explicit null ──────────────────────────

{
  // Triple-state patch: `null` is what clears a stored endpoint.
  assert.deepEqual(updateEndpointField("systemone", "/dropped"), { api_endpoint: null })
  assert.deepEqual(updateEndpointField("chat", "/dropped"), { api_endpoint: null })
  assert.deepEqual(updateEndpointField("image", " /images/generations "), {
    api_endpoint: "/images/generations",
  })
}

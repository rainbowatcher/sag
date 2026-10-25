import assert from "node:assert/strict"
import type { Provider, TargetItem } from "../types/api.ts"
import {
  applyPickerSelection,
  buildPickerGroups,
  diffSelection,
  filterPickerGroups,
  flattenOptions,
  isPickedRow,
  optionKey,
  selectedKeysFromTargets,
} from "./targetPicker.ts"

// Boundary tests for the route target picker's set semantics.
//
// The picker edits targets as a set of provider+model pairs: existing rows are
// pre-ticked, un-ticking removes the row, ticking appends one, and rows the
// picker cannot represent (empty provider/model) must survive untouched.

function provider(partial: Partial<Provider> & { name: string }): Provider {
  return {
    api_keys: [],
    key_strategy: "fill_first",
    apis: {} as Provider["apis"],
    timeout_secs: 60,
    headers: {},
    role_mappings: {},
    body_overrides: {},
    strip_fields: [],
    model_strip_fields: {},
    model_overrides: {},
    enabled_models: [],
    enabled: true,
    proxy_mode: "inherit",
    ...partial,
  }
}

function target(partial: Partial<TargetItem> & { id: number }): TargetItem {
  return {
    provider: "openai",
    real_model: "gpt-4o",
    priority: 0,
    retries: 0,
    extra: "",
    ...partial,
  }
}

const openai = provider({ name: "openai", enabled_models: ["gpt-4o", "gpt-4o-mini"] })
const anthropic = provider({ name: "anthropic", enabled: false, enabled_models: ["claude-3"] })

// ── pair identity: same model on two providers is two options ──────
{
  assert.notEqual(
    optionKey("openai", "gpt-4o"),
    optionKey("anthropic", "gpt-4o"),
    "the same model id under two providers is a distinct option",
  )
  assert.equal(optionKey("openai", "gpt-4o"), optionKey("openai", "gpt-4o"))
}

// ── buildPickerGroups: provider order, disabled flag, orphan rows ──
{
  const groups = buildPickerGroups(
    [openai, anthropic],
    { openai: ["gpt-4o", "gpt-4o-mini"], anthropic: ["claude-3"] },
    [target({ id: 1, provider: "openai", real_model: "gpt-4o-retired" })],
  )

  assert.deepEqual(
    groups.map((g) => g.provider),
    ["openai", "anthropic"],
    "groups follow the provider config order",
  )
  assert.deepEqual(
    groups[0].options.map((o) => o.model),
    ["gpt-4o", "gpt-4o-mini", "gpt-4o-retired"],
    "served models first, then target-only models appended",
  )
  assert.equal(groups[0].options[2].orphan, true, "target-only model is flagged orphan")
  assert.equal(groups[0].options[0].orphan, false, "served model is not orphan")
  assert.equal(groups[1].disabledProvider, true, "disabled provider is flagged")
  assert.equal(groups[1].options[0].disabledProvider, true)
}

// ── buildPickerGroups: unknown provider + unrepresentable rows ─────
{
  const groups = buildPickerGroups([openai], { openai: ["gpt-4o"] }, [
    target({ id: 1, provider: "gone", real_model: "old-model" }),
    // Mid-edit rows (empty provider or model) are not pickable options.
    target({ id: 2, provider: "", real_model: "" }),
    target({ id: 3, provider: "openai", real_model: "" }),
  ])

  assert.deepEqual(
    groups.map((g) => g.provider),
    ["openai", "gone"],
    "a provider that only an existing target references gets its own group",
  )
  assert.deepEqual(
    groups[1].options.map((o) => o.model),
    ["old-model"],
    "the deleted provider's row stays selectable so it can be un-ticked",
  )
  assert.equal(groups[0].options.length, 1, "empty-model rows are not offered as options")

  assert.equal(isPickedRow(target({ id: 9, provider: "", real_model: "gpt-4o" })), false)
  assert.equal(isPickedRow(target({ id: 9, provider: "openai", real_model: "" })), false)
  assert.equal(isPickedRow(target({ id: 9 })), true)
}

// ── buildPickerGroups: empty providers keep their section ────────
{
  const groups = buildPickerGroups([openai, provider({ name: "empty" })], {}, [])
  assert.deepEqual(
    groups.map((g) => g.provider),
    ["openai", "empty"],
    "a provider without any model still renders its section (empty state)",
  )
  assert.deepEqual(groups[1].options, [], "the empty provider offers no options")
  assert.deepEqual(
    flattenOptions(groups).map((o) => o.model),
    [],
    "option-less groups contribute nothing to the navigation order",
  )
  assert.deepEqual(
    filterPickerGroups(groups, "").map((g) => g.provider),
    ["openai", "empty"],
    "the empty section survives an empty query so its hint stays visible",
  )
  assert.deepEqual(
    filterPickerGroups(groups, "empty").map((g) => g.provider),
    [],
    "an option-less group is dropped while filtering (nothing can match)",
  )
}

// ── selectedKeysFromTargets: dedup, skip unrepresentable ──────────
{
  const keys = selectedKeysFromTargets([
    target({ id: 1 }),
    target({ id: 2 }), // duplicate pair
    target({ id: 3, provider: "anthropic", real_model: "claude-3" }),
    target({ id: 4, provider: "", real_model: "" }),
  ])
  assert.deepEqual(keys, [optionKey("openai", "gpt-4o"), optionKey("anthropic", "claude-3")])
}

// ── filterPickerGroups: provider name and model name both hit ─────
{
  const groups = buildPickerGroups(
    [openai, anthropic],
    { openai: ["gpt-4o", "gpt-4o-mini"], anthropic: ["claude-3"] },
    [],
  )

  assert.deepEqual(
    filterPickerGroups(groups, "").map((g) => g.provider),
    ["openai", "anthropic"],
    "empty query keeps every group (so headings stay visible)",
  )
  assert.deepEqual(
    filterPickerGroups(groups, "   ").map((g) => g.provider),
    ["openai", "anthropic"],
    "whitespace-only query is treated as empty",
  )

  const byModel = filterPickerGroups(groups, "MINI")
  assert.deepEqual(
    byModel.map((g) => [g.provider, g.options.map((o) => o.model)]),
    [["openai", ["gpt-4o-mini"]]],
    "model name matches case-insensitively",
  )

  const byProvider = filterPickerGroups(groups, "anthropic")
  assert.deepEqual(
    byProvider.map((g) => [g.provider, g.options.map((o) => o.model)]),
    [["anthropic", ["claude-3"]]],
    "a provider-name hit keeps all of that provider's models",
  )

  assert.deepEqual(filterPickerGroups(groups, "nope"), [], "no match drops every group")
}

// ── flattenOptions: list / keyboard order ─────────────────────────
{
  const groups = buildPickerGroups(
    [openai, anthropic],
    { openai: ["gpt-4o", "gpt-4o-mini"], anthropic: ["claude-3"] },
    [],
  )
  assert.deepEqual(
    flattenOptions(groups).map((o) => o.key),
    [
      optionKey("openai", "gpt-4o"),
      optionKey("openai", "gpt-4o-mini"),
      optionKey("anthropic", "claude-3"),
    ],
  )
}

// ── diffSelection: add / remove / no-op ───────────────────────────
{
  const groups = buildPickerGroups(
    [openai, anthropic],
    { openai: ["gpt-4o", "gpt-4o-mini"], anthropic: ["claude-3"] },
    [],
  )
  const current = [target({ id: 1, provider: "openai", real_model: "gpt-4o" })]

  const unchanged = diffSelection(current, selectedKeysFromTargets(current), groups)
  assert.deepEqual(unchanged.added, [])
  assert.deepEqual(unchanged.removed, [], "re-confirming the same set changes nothing")

  const added = diffSelection(
    current,
    [
      optionKey("openai", "gpt-4o"),
      optionKey("anthropic", "claude-3"),
      optionKey("openai", "gpt-4o-mini"),
    ],
    groups,
  )
  assert.deepEqual(
    added.added.map((o) => o.key),
    [optionKey("openai", "gpt-4o-mini"), optionKey("anthropic", "claude-3")],
    "new pairs are ordered by picker order, not click order",
  )
  assert.deepEqual(added.removed, [])

  const removed = diffSelection(current, [], groups)
  assert.deepEqual(
    removed.removed.map((t) => t.id),
    [1],
    "un-ticking every row drops them all",
  )
  assert.deepEqual(removed.added, [])
}

// ── diffSelection: duplicate rows and unrepresentable rows ────────
{
  const groups = buildPickerGroups([openai], { openai: ["gpt-4o"] }, [])
  const current = [
    target({ id: 1, provider: "openai", real_model: "gpt-4o" }),
    target({ id: 2, provider: "openai", real_model: "gpt-4o" }),
    target({ id: 3, provider: "", real_model: "" }),
  ]

  const kept = diffSelection(current, [optionKey("openai", "gpt-4o")], groups)
  assert.deepEqual(kept.removed, [], "duplicate rows of a ticked pair are all kept")

  const dropped = diffSelection(current, [], groups)
  assert.deepEqual(
    dropped.removed.map((t) => t.id),
    [1, 2],
    "an unticked pair drops every row carrying it; mid-edit rows are untouched",
  )
}

// ── applyPickerSelection: keep settings, append defaults ──────────
{
  const groups = buildPickerGroups(
    [openai, anthropic],
    { openai: ["gpt-4o", "gpt-4o-mini"], anthropic: ["claude-3"] },
    [],
  )
  const current = [
    target({
      id: 1,
      provider: "openai",
      real_model: "gpt-4o",
      priority: 7,
      retries: 2,
      extra: '{"model_id":39}',
    }),
    target({ id: 2, provider: "openai", real_model: "gpt-4o-mini", priority: 3 }),
    target({ id: 3, provider: "", real_model: "", priority: 0 }),
  ]

  const next = applyPickerSelection(
    current,
    [optionKey("openai", "gpt-4o"), optionKey("anthropic", "claude-3")],
    groups,
    4,
  )

  assert.deepEqual(
    next.map((t) => [t.id, t.provider, t.real_model]),
    [
      [1, "openai", "gpt-4o"],
      [3, "", ""],
      [4, "anthropic", "claude-3"],
    ],
    "kept rows stay in place, dropped rows go away, new pairs append",
  )
  assert.equal(next[0].priority, 7, "per-row settings of kept rows survive")
  assert.equal(next[0].retries, 2)
  assert.equal(next[0].extra, '{"model_id":39}')
  assert.equal(next[2].priority, 0, "appended rows use the add-row defaults")
  assert.equal(next[2].retries, 0)
  assert.equal(next[2].extra, "")
}

// ── applyPickerSelection: sequential ids from nextId ──────────────
{
  const groups = buildPickerGroups([openai], { openai: ["gpt-4o", "gpt-4o-mini", "o3"] }, [])
  const next = applyPickerSelection(
    [],
    [optionKey("openai", "gpt-4o"), optionKey("openai", "gpt-4o-mini"), optionKey("openai", "o3")],
    groups,
    11,
  )
  assert.deepEqual(
    next.map((t) => t.id),
    [11, 12, 13],
    "ids continue from the caller-supplied nextId",
  )
}

// ── applyPickerSelection: clearing everything ────────────────────
{
  const groups = buildPickerGroups([openai], { openai: ["gpt-4o"] }, [])
  const next = applyPickerSelection(
    [target({ id: 1 }), target({ id: 2, provider: "", real_model: "" })],
    [],
    groups,
    3,
  )
  assert.deepEqual(
    next.map((t) => t.id),
    [2],
    "un-ticking everything leaves only the rows the picker cannot represent",
  )
}

console.log("targetPicker.test: all assertions passed")

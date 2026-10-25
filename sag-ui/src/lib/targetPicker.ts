import type { Provider, TargetItem } from "@/types/api"

/**
 * Provider+model pair selection logic for the route target picker.
 *
 * The picker edits the target list as a *set* of provider+model pairs (the
 * magpie "Add a model" semantics): every existing target is pre-ticked,
 * clicking a ticked row unticks it, and confirming turns the set difference
 * into rows — additions append new rows, unticked existing rows are dropped,
 * and untouched rows keep their per-row priority/retries/extra settings.
 *
 * The same model name served by two providers is two distinct options, so the
 * selection key is the pair, never the bare model id.
 *
 * Pure functions and type-only imports only, so the node test runner can load
 * this file directly through `node --experimental-strip-types`.
 */

/** Separator that cannot occur in a provider name or in a model id. */
const KEY_SEPARATOR = "\u0000"

/** A provider+model pair — the unit of selection. */
export interface ModelOption {
  provider: string
  model: string
}

/** A selectable row in the picker. */
export interface PickerOption extends ModelOption {
  /** `provider\0model`, the set key used for selection. */
  key: string
  /** The model is missing from the provider's served-model list. */
  orphan: boolean
  /** The provider is disabled in the gateway config. */
  disabledProvider: boolean
}

/**
 * One provider section of the picker list. A group may have zero options: the
 * provider is still listed (with an empty-state hint) so the picker keeps its
 * "browse every provider" promise instead of hiding providers whose model list
 * is empty — `/api/providers/{name}/models` only echoes the configured
 * `enabled_models`, so an empty list is a reachable state, not a failure.
 */
export interface PickerGroup {
  provider: string
  disabledProvider: boolean
  options: PickerOption[]
}

/** What confirming the current selection would change. */
export interface SelectionDiff {
  /** Pairs to append as new rows, in picker (provider → model) order. */
  added: PickerOption[]
  /** Existing rows to drop. */
  removed: TargetItem[]
}

/** Stable set key for a provider+model pair. */
export function optionKey(provider: string, model: string): string {
  return `${provider}${KEY_SEPARATOR}${model}`
}

/**
 * Whether a target row is representable in the picker — it needs both a
 * provider and a model. Rows with an empty provider/model are mid-edit
 * leftovers; the picker leaves them untouched instead of deleting them.
 */
export function isPickedRow(target: TargetItem): boolean {
  return target.provider.trim() !== "" && target.real_model.trim() !== ""
}

/** Set keys of the rows a picker would pre-tick. */
export function selectedKeysFromTargets(targets: TargetItem[]): string[] {
  const keys: string[] = []
  for (const target of targets) {
    if (!isPickedRow(target)) continue
    const key = optionKey(target.provider, target.real_model)
    if (!keys.includes(key)) keys.push(key)
  }
  return keys
}

/**
 * Build the provider-grouped option list.
 *
 * Provider order follows the config order; every configured provider gets a
 * group even when it serves no models, so its section renders with an empty
 * state rather than silently disappearing. A provider referenced only by an
 * existing target (deleted provider, or a model list that failed to load)
 * gets an extra group at the end so those rows stay un-tickable instead of
 * becoming unremovable. Existing target models that the provider no longer
 * serves are appended to their group and flagged `orphan`.
 */
export function buildPickerGroups(
  providers: Provider[],
  providerModels: Record<string, string[]>,
  targets: TargetItem[],
): PickerGroup[] {
  const groups: PickerGroup[] = []
  const byProvider = new Map<string, PickerGroup>()

  function group(provider: string): PickerGroup {
    const existing = byProvider.get(provider)
    if (existing) return existing
    const info = providers.find((p) => p.name === provider)
    const created: PickerGroup = {
      provider,
      disabledProvider: info ? !info.enabled : false,
      options: [],
    }
    byProvider.set(provider, created)
    groups.push(created)
    return created
  }

  function push(target: PickerGroup, model: string, orphan: boolean): void {
    if (model.trim() === "") return
    const key = optionKey(target.provider, model)
    if (target.options.some((o) => o.key === key)) return
    target.options.push({
      provider: target.provider,
      model,
      key,
      orphan,
      disabledProvider: target.disabledProvider,
    })
  }

  for (const provider of providers) {
    const section = group(provider.name)
    for (const model of providerModels[provider.name] ?? []) {
      push(section, model, false)
    }
  }

  for (const target of targets) {
    if (!isPickedRow(target)) continue
    push(group(target.provider), target.real_model, true)
  }

  return groups
}

/** Flatten the groups into list order (the keyboard navigation order). */
export function flattenOptions(groups: PickerGroup[]): PickerOption[] {
  const options: PickerOption[] = []
  for (const group of groups) options.push(...group.options)
  return options
}

/**
 * Filter by provider name *or* model name. An empty query returns the groups
 * unchanged (so group headings — including the empty-provider ones — stay
 * visible); otherwise option-less groups are dropped, since an empty section
 * cannot match a needle. A provider-name hit keeps all of that provider's
 * models.
 */
export function filterPickerGroups(groups: PickerGroup[], query: string): PickerGroup[] {
  const needle = query.trim().toLowerCase()
  if (needle === "") return groups
  const filtered: PickerGroup[] = []
  for (const group of groups) {
    const providerMatches = group.provider.toLowerCase().includes(needle)
    const options = providerMatches
      ? group.options
      : group.options.filter((o) => o.model.toLowerCase().includes(needle))
    if (options.length > 0) filtered.push({ ...group, options })
  }
  return filtered
}

/**
 * Compare the ticked set against the current rows: which pairs are new and
 * which rows would be dropped. Rows the picker cannot represent are ignored.
 */
export function diffSelection(
  targets: TargetItem[],
  selectedKeys: string[],
  groups: PickerGroup[],
): SelectionDiff {
  const selected = new Set(selectedKeys)
  const existing = new Set(selectedKeysFromTargets(targets))
  const removed = targets.filter(
    (t) => isPickedRow(t) && !selected.has(optionKey(t.provider, t.real_model)),
  )
  const added = flattenOptions(groups).filter((o) => selected.has(o.key) && !existing.has(o.key))
  return { added, removed }
}

/**
 * Turn the ticked set into the next target list: kept rows stay in place with
 * their settings, new pairs are appended in picker order with the same
 * defaults the old "add target" button used, and unticked rows are dropped.
 *
 * `nextId` is the id to give the first appended row (callers pass
 * `max(existing ids) + 1`, mirroring the previous row-insertion behaviour).
 */
export function applyPickerSelection(
  targets: TargetItem[],
  selectedKeys: string[],
  groups: PickerGroup[],
  nextId: number,
): TargetItem[] {
  const selected = new Set(selectedKeys)
  const kept = targets.filter(
    (t) => !isPickedRow(t) || selected.has(optionKey(t.provider, t.real_model)),
  )
  const { added } = diffSelection(targets, selectedKeys, groups)
  let id = nextId
  const appended = added.map((option) => ({
    id: id++,
    provider: option.provider,
    real_model: option.model,
    priority: 0,
    retries: 0,
    extra: "",
  }))
  return [...kept, ...appended]
}

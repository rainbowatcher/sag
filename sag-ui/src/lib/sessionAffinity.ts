import type { SessionAffinityEntry } from "@/types/api"

/**
 * Display prefix of a hashed session key. The full 32-hex key is what
 * `DELETE /api/session-affinity` expects, so the popover keeps it in the
 * element's `title` while showing only the leading characters.
 */
export function shortSessionKey(key: string, length = 8): string {
  return key.slice(0, Math.max(0, length))
}

/**
 * Whether a session pin is still live. `null` means the route's TTL is `0`
 * (pins never expire); a positive countdown is live; a `<= 0` leftover is an
 * expired residue that should render greyed out.
 */
export function isPinActive(entry: SessionAffinityEntry): boolean {
  return entry.expires_in_secs === null || entry.expires_in_secs > 0
}

/**
 * Merge one fetch response into the shared pin list.
 *
 * The store behind the row popovers is shared by every route row, so a
 * `{ model }` fetch may only replace that model's slice. Assigning the whole
 * response would blank the rows fetched before it (hover row A → row B →
 * row A showed a false "no pins"). Omitting `model` is a full-space fetch and
 * replaces the list.
 *
 * Neither argument is mutated: a scoped merge copies the kept entries and the
 * incoming ones into a new array, while a full-space fetch returns `incoming`
 * as given (it already is the whole list).
 */
export function mergeSessionEntries(
  prev: SessionAffinityEntry[],
  incoming: SessionAffinityEntry[],
  model?: string,
): SessionAffinityEntry[] {
  if (model === undefined) return incoming
  return [...prev.filter((e) => e.model !== model), ...incoming]
}

/**
 * Group pins for display, strictly by liveness: all active pins first, then
 * the expired ones. The sort is stable, so entries inside each group keep
 * their original relative order (the backend's returned order).
 */
export function sortPins(entries: SessionAffinityEntry[]): SessionAffinityEntry[] {
  return [...entries].sort((a, b) => Number(isPinActive(b)) - Number(isPinActive(a)))
}

/**
 * The failure message that belongs to `model`, or `null` when there is none.
 *
 * The store is shared by every route row, so failures are stored per fetch
 * scope (`""` is the full-space fetch): one row's failure must never be
 * rendered by another row, hence the plain key lookup.
 *
 * The lookup is own-property only: a bare `errors[model]` would walk the
 * prototype chain, so a route model literally named `constructor` or
 * `toString` would render a `Function` object as its failure message.
 */
export function errorForModel(errors: Record<string, string>, model: string): string | null {
  return Object.hasOwn(errors, model) ? errors[model] : null
}

/**
 * Whether `model`'s fetch is in flight.
 *
 * Same per-scope lookup and the same own-property guard as `errorForModel`: a
 * bare `loading[model] ?? false` would walk the prototype chain, so a route
 * model literally named `constructor` / `toString` would read a `Function`
 * (non-nullish, so `?? false` never kicks in) and spin forever.
 */
export function loadingForModel(loading: Record<string, boolean>, model: string): boolean {
  return Object.hasOwn(loading, model) ? loading[model] : false
}

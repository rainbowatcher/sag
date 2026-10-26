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
 * A failed pin fetch, tagged with the model space it belongs to.
 *
 * The store is shared by every route row, so one row's failure must not be
 * rendered by another row: `scope` is the `model` the fetch asked for
 * (`undefined` for a full-space fetch) and only a matching row reads it.
 */
export interface SessionAffinityError {
  scope?: string
  message: string
}

/**
 * The failure message that belongs to `model`, or `null` when the stored
 * error came from another row's fetch (or there is none).
 */
export function errorForModel(error: SessionAffinityError | null, model: string): string | null {
  if (error === null) return null
  return error.scope === model ? error.message : null
}

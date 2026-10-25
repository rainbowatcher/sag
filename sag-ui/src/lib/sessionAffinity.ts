/**
 * Display prefix of a hashed session key. The full 32-hex key is what
 * `DELETE /api/session-affinity` expects, so the popover keeps it in the
 * element's `title` while showing only the leading characters.
 */
export function shortSessionKey(key: string, length = 8): string {
  return key.slice(0, Math.max(0, length))
}

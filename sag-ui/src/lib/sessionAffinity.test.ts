import assert from "node:assert/strict"
import { shortSessionKey } from "./sessionAffinity.ts"

// The popover shows only the leading characters of a hashed session key; the
// full 32-hex value stays in the `title`/DELETE body. These tests pin the
// truncation so a wrong slice can't send a truncated key back to
// `DELETE /api/session-affinity`.

// ── default prefix length ──────────────────────────────────────────
{
  const key = "0123456789abcdef0123456789abcdef"
  assert.equal(shortSessionKey(key), "01234567", "8 leading hex chars")
  assert.equal(shortSessionKey(key).length, 8)
}

// ── short / empty keys stay intact ─────────────────────────────────
{
  assert.equal(shortSessionKey(""), "", "empty key renders as empty string")
  assert.equal(shortSessionKey("abc"), "abc", "a key shorter than the prefix is not padded")
}

// ── custom length is honoured (never negative) ─────────────────────
{
  const key = "0123456789abcdef0123456789abcdef"
  assert.equal(shortSessionKey(key, 4), "0123")
  assert.equal(shortSessionKey(key, 0), "")
  assert.equal(shortSessionKey(key, -3), "", "a negative length yields an empty string")
  assert.equal(shortSessionKey(key, 64), key, "length beyond the key returns the whole key")
}

console.log("# sessionAffinity.test: all assertions passed")

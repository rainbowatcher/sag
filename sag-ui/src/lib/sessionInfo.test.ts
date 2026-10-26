import assert from "node:assert/strict"
import {
  SESSION_PLACEHOLDER,
  formatSessionTime,
  sessionPreviewText,
  sessionSourceKind,
  sessionText,
} from "./sessionInfo.ts"

// The session info card renders raw `SessionInfo` registry fields. These tests
// pin the display formatting so a missing field, an unknown source or a
// multi-line preview can never leak into the layout as an empty or wrapped cell.

// ── session_source narrowing ───────────────────────────────────────
{
  assert.equal(sessionSourceKind("meta"), "meta")
  assert.equal(sessionSourceKind("top"), "top")
  assert.equal(sessionSourceKind("pc"), "pc")
  assert.equal(sessionSourceKind("fb"), "fb")
  assert.equal(sessionSourceKind("  TOP "), "top", "trimmed and case-insensitive")
  assert.equal(sessionSourceKind("header"), null, "unknown source has no label")
  assert.equal(sessionSourceKind(""), null)
  assert.equal(sessionSourceKind(null), null)
  assert.equal(sessionSourceKind(undefined), null)
}

// ── optional field placeholder ─────────────────────────────────────
{
  assert.equal(sessionText("conversation-1"), "conversation-1")
  assert.equal(sessionText("  padded  "), "padded")
  assert.equal(sessionText(null), SESSION_PLACEHOLDER, "null renders the placeholder")
  assert.equal(sessionText(undefined), SESSION_PLACEHOLDER)
  assert.equal(sessionText(""), SESSION_PLACEHOLDER)
  assert.equal(sessionText("   "), SESSION_PLACEHOLDER, "blank-only value renders the placeholder")
}

// ── first-message preview ──────────────────────────────────────────
{
  assert.equal(sessionPreviewText("how do I rotate keys?"), "how do I rotate keys?")
  assert.equal(
    sessionPreviewText("line one\n\tline two   three"),
    "line one line two three",
    "whitespace runs collapse to a single space",
  )
  assert.equal(sessionPreviewText("   padded   "), "padded")
  assert.equal(sessionPreviewText(null), SESSION_PLACEHOLDER)
  assert.equal(sessionPreviewText(""), SESSION_PLACEHOLDER)
  assert.equal(sessionPreviewText(" \n "), SESSION_PLACEHOLDER, "whitespace-only preview is empty")
  assert.equal(sessionPreviewText("abcdef", 6), "abcdef", "a preview at the limit is untouched")
  assert.equal(sessionPreviewText("abcdef", 4), "abcd…", "longer previews are cut with an ellipsis")
  assert.equal(sessionPreviewText("abcdef", 0), "a…", "the limit never drops below one character")
}

// ── ISO timestamp formatting ───────────────────────────────────────
{
  const pad = (n: number) => String(n).padStart(2, "0")
  const iso = "2026-08-20T02:03:04Z"
  const d = new Date(iso)
  const expected = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  assert.equal(formatSessionTime(iso), expected, "local date + 24h time, no comma")
  assert.match(formatSessionTime(iso), /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/)
  assert.equal(formatSessionTime(""), SESSION_PLACEHOLDER)
  assert.equal(formatSessionTime(null), SESSION_PLACEHOLDER)
  assert.equal(formatSessionTime(undefined), SESSION_PLACEHOLDER)
  assert.equal(formatSessionTime("not-a-date"), SESSION_PLACEHOLDER, "unparseable input degrades")
  assert.equal(
    formatSessionTime(iso),
    formatSessionTime(iso),
    "repeated calls render the same string",
  )
  assert.notEqual(
    formatSessionTime("2026-08-20T03:03:04Z"),
    formatSessionTime("2026-08-20T02:03:04Z"),
    "an hour apart renders differently",
  )
}

console.log("# sessionInfo.test: all assertions passed")

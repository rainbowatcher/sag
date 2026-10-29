import assert from "node:assert/strict"
import type { PrivacyConfigSnapshot } from "../types/api.ts"
import {
  buildPrivacyPayload,
  createPrivacyDraft,
  getPrivacyChanges,
  validatePrivacyDraft,
} from "./privacyConfig.ts"

// Draft/snapshot boundary tests for the settings > privacy form.
//
// The form edits a plain draft (textareas for the two word lists, rows with
// local UI ids for the custom regexes) and only the confirmed snapshot is the
// server truth. These tests pin the three conversions the component relies on:
// create (snapshot -> draft, faithful and isolated), getPrivacyChanges (dirty
// detection without false positives) and buildPrivacyPayload (what is sent).

function snapshot(partial: Partial<PrivacyConfigSnapshot> = {}): PrivacyConfigSnapshot {
  return {
    enabled: true,
    mode: "mask",
    restore: true,
    builtins: {
      email: true,
      phone: true,
      id_card_cn: true,
      bank_card: true,
      iban: true,
      api_key: true,
      private_key: true,
    },
    custom_regexes: [],
    custom_words: { items: [] },
    allowlist: { items: [] },
    salt_configured: false,
    ...partial,
  }
}

// ── createPrivacyDraft: first load is clean ────────────────────────
{
  const saved = snapshot({
    custom_regexes: [
      { kind: "email", pattern: "^a+$", case_insensitive: false },
      { kind: "email", pattern: "b", case_insensitive: true },
    ],
    custom_words: { items: ["alpha", "beta"] },
    allowlist: { items: ["gamma"] },
  })
  const draft = createPrivacyDraft(saved)

  assert.deepEqual(
    getPrivacyChanges(draft, saved),
    [],
    "a draft built from the snapshot has no changes (no false dirty on load)",
  )
  assert.equal(draft.newSalt, "", "the salt input always starts empty")
  assert.deepEqual(
    draft.regexes.map((row) => row.id),
    ["saved-0", "saved-1"],
    "saved rows get index-derived, per-row ids",
  )
  assert.equal(draft.customWordsText, "alpha\nbeta")
  assert.equal(draft.allowlistText, "gamma")
}

// ── createPrivacyDraft: deep isolation in both directions ──────────
{
  const saved = snapshot({
    custom_regexes: [{ kind: "email", pattern: "x", case_insensitive: false }],
    custom_words: { items: ["alpha"] },
  })
  const draft = createPrivacyDraft(saved)

  draft.enabled = false
  draft.builtins.email = false
  draft.regexes[0].kind = "phone"
  draft.regexes[0].case_insensitive = true
  draft.regexes.push({ id: "new-1", kind: "k", pattern: "p", case_insensitive: false })
  draft.customWordsText = "changed"

  assert.equal(saved.enabled, true, "draft scalars do not write through")
  assert.equal(saved.builtins.email, true, "draft.builtins is a copy")
  assert.equal(saved.custom_regexes[0].kind, "email", "draft regex rows are copies")
  assert.equal(saved.custom_regexes[0].case_insensitive, false)
  assert.equal(saved.custom_regexes.length, 1, "pushing a draft row does not grow the snapshot")
  assert.equal(saved.custom_words.items[0], "alpha")

  // The snapshot is the confirmed server truth; changing it afterwards must not
  // leak into the live draft either.
  saved.mode = "block"
  saved.builtins.phone = false
  saved.custom_regexes[0].pattern = "y"
  saved.custom_words.items.push("beta")
  assert.equal(draft.mode, "mask")
  assert.equal(draft.builtins.phone, true)
  assert.equal(draft.regexes[0].pattern, "x")
  assert.equal(draft.customWordsText, "changed")
}

// ── getPrivacyChanges: one entry per edited field, stable order ────
{
  const saved = snapshot({ custom_words: { items: ["alpha"] }, allowlist: { items: ["beta"] } })

  const enabled = createPrivacyDraft(saved)
  enabled.enabled = false
  assert.deepEqual(getPrivacyChanges(enabled, saved), ["enabled"])

  const mode = createPrivacyDraft(saved)
  mode.mode = "block"
  assert.deepEqual(getPrivacyChanges(mode, saved), ["mode"], "restore is not implied by mode")

  const restore = createPrivacyDraft(saved)
  restore.restore = false
  assert.deepEqual(getPrivacyChanges(restore, saved), ["restore"], "restore is independent")

  const builtins = createPrivacyDraft(saved)
  builtins.builtins.api_key = false
  assert.deepEqual(getPrivacyChanges(builtins, saved), ["builtins"])

  const kind = createPrivacyDraft(saved)
  kind.regexes.push({ id: "new-1", kind: "email", pattern: "x", case_insensitive: false })
  assert.deepEqual(getPrivacyChanges(kind, saved), ["custom_regexes"])

  const flag = createPrivacyDraft(saved)
  flag.regexes.push({ id: "new-1", kind: "email", pattern: "x", case_insensitive: true })
  assert.deepEqual(getPrivacyChanges(flag, saved), ["custom_regexes"], "flags are compared too")

  const words = createPrivacyDraft(saved)
  words.customWordsText = "alpha\nbeta"
  assert.deepEqual(getPrivacyChanges(words, saved), ["custom_words"])
  assert.notDeepEqual(
    getPrivacyChanges(words, saved),
    ["custom_words", "allowlist"],
    "editing one word list leaves the other clean",
  )

  const allowlist = createPrivacyDraft(saved)
  allowlist.allowlistText = "beta\ngamma"
  assert.deepEqual(getPrivacyChanges(allowlist, saved), ["allowlist"])

  const salt = createPrivacyDraft(saved)
  salt.newSalt = "s3cret"
  assert.deepEqual(getPrivacyChanges(salt, saved), ["salt"])

  const everything = createPrivacyDraft(saved)
  everything.enabled = false
  everything.mode = "warn"
  everything.restore = false
  everything.builtins.email = false
  everything.regexes.push({ id: "new-1", kind: "email", pattern: "x", case_insensitive: false })
  everything.customWordsText = "alpha\nbeta"
  everything.allowlistText = "beta\ngamma"
  everything.newSalt = "s3cret"
  assert.deepEqual(
    getPrivacyChanges(everything, saved),
    [
      "enabled",
      "mode",
      "restore",
      "builtins",
      "custom_regexes",
      "custom_words",
      "allowlist",
      "salt",
    ],
    "changes follow the payload field order with salt last",
  )
}

// ── word lists: an untouched list keeps the saved bytes verbatim ───
{
  // Duplicates, order, padding and an embedded newline must all survive an
  // untouched round trip: the snapshot is the only source of truth until the
  // user actually edits the textarea.
  const items = [" second ", "first\nsecond", "dup", "dup", ""]
  const saved = snapshot({ custom_words: { items }, allowlist: { items } })
  const draft = createPrivacyDraft(saved)

  assert.deepEqual(getPrivacyChanges(draft, saved), [], "joining items for display is not a change")

  const payload = buildPrivacyPayload(draft, saved)
  assert.deepEqual(
    payload.custom_words.items,
    items,
    "untouched custom_words keeps bytes, order and duplicates (including an embedded newline)",
  )
  assert.deepEqual(payload.allowlist.items, items, "untouched allowlist behaves the same")
  assert.notEqual(
    payload.custom_words.items,
    saved.custom_words.items,
    "the payload owns its arrays instead of aliasing the snapshot",
  )
  assert.notEqual(
    payload.custom_words.items,
    draft.customWordsText,
    "the payload never aliases draft storage",
  )
}

// ── word lists: editing normalizes by line ─────────────────────────
{
  const saved = snapshot({ custom_words: { items: [] } })
  const draft = createPrivacyDraft(saved)
  draft.customWordsText = "\n  alpha \n\n beta\nbeta \t\n"
  draft.allowlistText = " only \n\n"

  assert.deepEqual(getPrivacyChanges(draft, saved), ["custom_words", "allowlist"])

  const payload = buildPrivacyPayload(draft, saved)
  assert.deepEqual(
    payload.custom_words.items,
    ["alpha", "beta", "beta"],
    "edited lines are trimmed, blanks dropped, duplicates and order kept",
  )
  assert.deepEqual(payload.allowlist.items, ["only"])
}

// ── custom regexes: required checks use trim, payload keeps bytes ──
{
  const saved = snapshot({
    custom_regexes: [{ kind: "email", pattern: " ^a+$ ", case_insensitive: false }],
  })
  const draft = createPrivacyDraft(saved)
  assert.deepEqual(
    getPrivacyChanges(draft, saved),
    [],
    "a saved pattern with surrounding spaces is not dirty on load",
  )

  // Required-ness is decided on the trimmed value only ...
  draft.regexes.push({ id: "new-1", kind: "  ", pattern: "\n\t", case_insensitive: false })
  assert.deepEqual(
    validatePrivacyDraft(draft),
    [
      { rowId: "new-1", field: "kind", code: "required" },
      { rowId: "new-1", field: "pattern", code: "required" },
    ],
    "blank rows report kind before pattern under their row id",
  )

  // ... but the payload sends the raw user text, padding included.
  draft.regexes[1].kind = " phone "
  draft.regexes[1].pattern = " \\d+ "
  assert.deepEqual(getPrivacyChanges(draft, saved), ["custom_regexes"])
  const payload = buildPrivacyPayload(draft, saved)
  assert.deepEqual(payload.custom_regexes, [
    { kind: "email", pattern: " ^a+$ ", case_insensitive: false },
    { kind: " phone ", pattern: " \\d+ ", case_insensitive: false },
  ])
  assert.equal("id" in payload.custom_regexes[0], false, "UI-only row ids are stripped")
}

// ── custom regexes: errors locate duplicated kinds by row id ───────
{
  const draft = createPrivacyDraft(snapshot())
  draft.regexes = [
    { id: "row-a", kind: "EMAIL", pattern: "  ", case_insensitive: false },
    { id: "row-b", kind: "", pattern: "", case_insensitive: false },
    { id: "row-c", kind: "EMAIL", pattern: "x", case_insensitive: false },
  ]
  const before = JSON.stringify(draft)

  assert.deepEqual(
    validatePrivacyDraft(draft),
    [
      { rowId: "row-a", field: "pattern", code: "required" },
      { rowId: "row-b", field: "kind", code: "required" },
      { rowId: "row-b", field: "pattern", code: "required" },
    ],
    "two rows share the kind EMAIL, so errors are keyed by the stable row id",
  )
  assert.equal(JSON.stringify(draft), before, "validation does not mutate the draft")
}

// ── salt: empty means "leave as is", non-empty goes verbatim ───────
{
  const saved = snapshot({ salt_configured: true })
  const draft = createPrivacyDraft(saved)
  assert.deepEqual(getPrivacyChanges(draft, saved), [], "empty salt input is never dirty")

  const empty = buildPrivacyPayload(draft, saved)
  assert.equal(
    Object.hasOwn(empty, "salt"),
    false,
    "an empty salt input omits the key entirely (keeps the current salt)",
  )

  draft.newSalt = " "
  assert.deepEqual(getPrivacyChanges(draft, saved), ["salt"], "a space is not empty")
  const spaced = buildPrivacyPayload(draft, saved)
  assert.equal(spaced.salt, " ", "a whitespace salt is sent as-is, not trimmed")
  assert.deepEqual(getPrivacyChanges(draft, saved), ["salt"])

  draft.newSalt = " pad "
  assert.equal(buildPrivacyPayload(draft, saved).salt, " pad ")
}

// ── payload shape: known fields only, no snapshot-only values ──────
{
  const saved = snapshot({
    salt_configured: true,
    custom_regexes: [{ kind: "email", pattern: "x", case_insensitive: false }],
    custom_words: { items: ["alpha"] },
    allowlist: { items: ["beta"] },
  })
  const draft = createPrivacyDraft(saved)
  const payload = buildPrivacyPayload(draft, saved)

  assert.deepEqual(
    Object.keys(payload).sort(),
    ["allowlist", "builtins", "custom_regexes", "custom_words", "enabled", "mode", "restore"],
    "the payload is the full writable section and nothing else",
  )
  assert.equal("salt_configured" in payload, false, "read-only salt state is never sent back")
  assert.equal("id" in payload.custom_regexes[0], false)

  // Mutating the payload must not reach the draft or the snapshot.
  payload.builtins.phone = false
  payload.custom_words.items.push("leak")
  payload.custom_regexes[0].kind = "leak"
  payload.mode = "block"
  assert.equal(draft.builtins.phone, true)
  assert.equal(draft.customWordsText, "alpha")
  assert.equal(draft.regexes[0].kind, "email")
  assert.equal(draft.mode, "mask")
  assert.equal(saved.builtins.phone, true)
  assert.deepEqual(saved.custom_words.items, ["alpha"])
  assert.equal(saved.custom_regexes[0].kind, "email")
  assert.equal(saved.mode, "mask")
}

// ── pure helpers never mutate their inputs ─────────────────────────
{
  const saved = snapshot({
    custom_regexes: [{ kind: "email", pattern: " x ", case_insensitive: false }],
    custom_words: { items: [" a ", "b"] },
  })
  const savedBefore = JSON.stringify(saved)
  const draft = createPrivacyDraft(saved)
  draft.enabled = false
  draft.regexes[0].pattern = " y "
  draft.customWordsText = " a \nb \n"
  draft.newSalt = "s"
  const draftBefore = JSON.stringify(draft)

  getPrivacyChanges(draft, saved)
  validatePrivacyDraft(draft)
  buildPrivacyPayload(draft, saved)

  assert.equal(JSON.stringify(saved), savedBefore, "the snapshot is untouched")
  assert.equal(JSON.stringify(draft), draftBefore, "the draft is untouched")
}

console.log("# privacyConfig.test: all assertions passed")

import type {
  PrivacyBuiltins,
  PrivacyConfigFields,
  PrivacyConfigSnapshot,
  PrivacyCustomRegex,
  PrivacyMode,
  UpdatePrivacyConfigPayload,
} from "../types/api.ts"

/**
 * Editable state of the settings > privacy form.
 *
 * The draft is plain data (no Vue reactivity here) built from the last
 * confirmed snapshot. The two word lists are edited as raw textarea text, and
 * every regex row carries a local-only `id` so the form can bind errors and
 * keys per row: saved rows get `saved-<index>`, rows the form appends get their
 * own `new-<sequence>`. Neither id is ever sent to the gateway.
 *
 * `newSalt` is the replacement salt input: empty means "keep the current salt"
 * (the snapshot only reports `salt_configured`, never the value itself).
 */
export interface PrivacyDraft {
  enabled: boolean
  mode: PrivacyMode
  restore: boolean
  builtins: PrivacyBuiltins
  customWordsText: string
  allowlistText: string
  regexes: (PrivacyCustomRegex & { id: string })[]
  newSalt: string
}

/** One required-field failure on a custom regex row. */
interface PrivacyFieldError {
  rowId: string
  field: "kind" | "pattern"
  code: "required"
}

type PrivacyRegexRow = PrivacyDraft["regexes"][number]

/** All built-in detector keys, in payload order. */
const BUILTIN_KEYS: readonly (keyof PrivacyBuiltins)[] = [
  "email",
  "phone",
  "id_card_cn",
  "bank_card",
  "iban",
  "api_key",
  "private_key",
]

function cloneBuiltins(source: PrivacyBuiltins): PrivacyBuiltins {
  return {
    email: source.email,
    phone: source.phone,
    id_card_cn: source.id_card_cn,
    bank_card: source.bank_card,
    iban: source.iban,
    api_key: source.api_key,
    private_key: source.private_key,
  }
}

function builtinsEqual(a: PrivacyBuiltins, b: PrivacyBuiltins): boolean {
  return BUILTIN_KEYS.every((key) => a[key] === b[key])
}

/**
 * A word list as the textarea shows it. Joining is display-only: an untouched
 * textarea is recognized by comparing against this exact string, which is how
 * a saved list keeps its original bytes (order, duplicates, items that contain
 * a newline).
 */
function itemsToText(items: string[]): string {
  return items.join("\n")
}

/** Split edited textarea text: trim each line, drop blanks, keep order and duplicates. */
function textToItems(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "")
}

/**
 * Effective word list of the draft: the saved items verbatim while the textarea
 * text is still byte-identical to its rendered form, otherwise the normalized
 * line split. Always returns a fresh array.
 */
function resolveWordList(text: string, savedItems: string[]): string[] {
  return text === itemsToText(savedItems) ? savedItems.slice() : textToItems(text)
}

function sameItems(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((item, index) => item === b[index])
}

function sameRegex(a: PrivacyCustomRegex, b: PrivacyCustomRegex): boolean {
  return (
    a.kind === b.kind &&
    a.pattern === b.pattern &&
    Boolean(a.case_insensitive) === Boolean(b.case_insensitive)
  )
}

/**
 * Compare rows against saved rules by position. `kind` may repeat, so there is
 * no identity beyond order; both sides are user text, compared raw (a pattern's
 * surrounding spaces are meaningful and must not read as an edit or vanish).
 */
function sameRegexes(rows: PrivacyRegexRow[], saved: PrivacyCustomRegex[]): boolean {
  return rows.length === saved.length && rows.every((row, index) => sameRegex(row, saved[index]))
}

/**
 * Build a form draft from the confirmed snapshot. Everything is copied (the
 * draft never aliases the snapshot) and `newSalt` starts empty so a freshly
 * loaded form is never dirty.
 */
export function createPrivacyDraft(saved: PrivacyConfigSnapshot): PrivacyDraft {
  return {
    enabled: saved.enabled,
    mode: saved.mode,
    restore: saved.restore,
    builtins: cloneBuiltins(saved.builtins),
    customWordsText: itemsToText(saved.custom_words.items),
    allowlistText: itemsToText(saved.allowlist.items),
    regexes: saved.custom_regexes.map((rule, index) => ({
      id: `saved-${index}`,
      kind: rule.kind,
      pattern: rule.pattern,
      case_insensitive: rule.case_insensitive,
    })),
    newSalt: "",
  }
}

/**
 * Changed writable fields, in payload order with `salt` last. Each field is
 * compared on its own (a mode change does not dirty `restore`); an empty salt
 * input is never a change, and an untouched word list is not dirtied by its
 * textarea rendering.
 */
export function getPrivacyChanges(
  draft: PrivacyDraft,
  saved: PrivacyConfigSnapshot,
): (keyof PrivacyConfigFields | "salt")[] {
  const changes: (keyof PrivacyConfigFields | "salt")[] = []
  if (draft.enabled !== saved.enabled) changes.push("enabled")
  if (draft.mode !== saved.mode) changes.push("mode")
  if (draft.restore !== saved.restore) changes.push("restore")
  if (!builtinsEqual(draft.builtins, saved.builtins)) changes.push("builtins")
  if (!sameRegexes(draft.regexes, saved.custom_regexes)) changes.push("custom_regexes")
  if (
    !sameItems(
      resolveWordList(draft.customWordsText, saved.custom_words.items),
      saved.custom_words.items,
    )
  ) {
    changes.push("custom_words")
  }
  if (
    !sameItems(resolveWordList(draft.allowlistText, saved.allowlist.items), saved.allowlist.items)
  ) {
    changes.push("allowlist")
  }
  if (draft.newSalt !== "") changes.push("salt")
  return changes
}

/**
 * Required-field failures of the custom regex rows, in row order (`kind` before
 * `pattern` within a row). Blankness is decided on the trimmed value only — the
 * text itself is never trimmed — and errors are keyed by row id because several
 * rows may share the same `kind`.
 */
export function validatePrivacyDraft(draft: PrivacyDraft): PrivacyFieldError[] {
  const errors: PrivacyFieldError[] = []
  for (const row of draft.regexes) {
    if (row.kind.trim() === "") errors.push({ rowId: row.id, field: "kind", code: "required" })
    if (row.pattern.trim() === "") {
      errors.push({ rowId: row.id, field: "pattern", code: "required" })
    }
  }
  return errors
}

/**
 * `PUT /api/config` body for the draft: the whole writable section (nested
 * objects included), fresh copies only. UI row ids and `salt_configured` are
 * dropped; regex strings and a non-empty salt go out exactly as typed. `salt`
 * is omitted while the input is empty, which keeps the current salt.
 */
export function buildPrivacyPayload(
  draft: PrivacyDraft,
  saved: PrivacyConfigSnapshot,
): UpdatePrivacyConfigPayload {
  const payload: UpdatePrivacyConfigPayload = {
    enabled: draft.enabled,
    mode: draft.mode,
    restore: draft.restore,
    builtins: cloneBuiltins(draft.builtins),
    custom_regexes: draft.regexes.map((row) => ({
      kind: row.kind,
      pattern: row.pattern,
      case_insensitive: row.case_insensitive,
    })),
    custom_words: { items: resolveWordList(draft.customWordsText, saved.custom_words.items) },
    allowlist: { items: resolveWordList(draft.allowlistText, saved.allowlist.items) },
  }
  if (draft.newSalt !== "") payload.salt = draft.newSalt
  return payload
}

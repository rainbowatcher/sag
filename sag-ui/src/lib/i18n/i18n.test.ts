import assert from "node:assert/strict"
import type { App } from "vue"
import { en } from "./en.ts"
import {
  DEFAULT_LOCALE,
  LOCALE_STORAGE_KEY,
  installI18n,
  setLocale,
  t,
  useI18n,
  type MessageKey,
} from "./index.ts"
import { zh } from "./zh.ts"

// Boundary tests for the DIY i18n layer. Run with the node test runner
// (`node --experimental-strip-types --test "src/**/*.test.ts"`), i.e. in a
// plain node environment with no `window`, no `document` and no
// `localStorage` — the module must stay usable there.

/** Drop a key from the live `zh` dictionary to exercise the en fallback. */
function withZhKeyRemoved(key: string, run: () => void): void {
  const dict = zh as Record<string, string | undefined>
  const saved = dict[key]
  delete dict[key]
  try {
    run()
  } finally {
    if (saved !== undefined) dict[key] = saved
  }
}

// ── defaults ────────────────────────────────────────────────────────
{
  assert.equal(DEFAULT_LOCALE, "zh", "Chinese is the default UI language")
  assert.equal(LOCALE_STORAGE_KEY, "sag-ui-lang")
  assert.equal(useI18n().locale.value, "zh", "a fresh process without localStorage starts on zh")
}

// ── interpolation: named {param} only ───────────────────────────────
{
  // No seeded key carries a placeholder yet, so interpolation is exercised
  // through the `rawFallback` migration valve.
  const missing = "common.not.seeded" as MessageKey
  assert.equal(t(missing, { name: "World" }, "Hello {name}"), "Hello World")
  assert.equal(t(missing, { a: 1, b: 2 }, "{a} + {b}"), "1 + 2", "numbers are stringified")
  assert.equal(t(missing, { name: "x" }, "{name} and {name}"), "x and x", "all occurrences")

  // A missing param keeps its literal placeholder while the rest resolves.
  assert.equal(t(missing, { other: "y" }, "Hello {name}, {other}"), "Hello {name}, y")
  // No params at all → the template is returned untouched.
  assert.equal(t(missing, undefined, "Hello {name}"), "Hello {name}")
  // A template without placeholders ignores params.
  assert.equal(t("common.save", { name: "x" }), "保存")
}

// ── fallback order: active locale → en → rawFallback → key ──────────
{
  setLocale("en")
  assert.equal(t("common.save"), "Save")
  setLocale("zh")
  assert.equal(t("common.save"), "保存")

  withZhKeyRemoved("common.save", () => {
    assert.equal(t("common.save"), "Save", "a key missing from zh falls back to en")
  })
  assert.equal(t("common.save"), "保存", "dictionary restored after the probe")

  const missing = "common.not.seeded" as MessageKey
  assert.equal(t(missing, undefined, "Hardcoded"), "Hardcoded", "en miss → rawFallback")
  assert.equal(t(missing), "common.not.seeded", "no fallback left → the key itself")
}

// ── key parity: en and zh cover exactly the same keys ───────────────
{
  assert.deepEqual(Object.keys(zh).sort(), Object.keys(en).sort())
  assert.ok(Object.keys(en).length > 0, "dictionary must not be empty")
  for (const [key, value] of Object.entries(en)) {
    assert.equal(typeof value, "string", `en.${key} must be a string`)
    assert.notEqual(value.trim(), "", `en.${key} must not be blank`)
  }
  for (const [key, value] of Object.entries(zh)) {
    assert.notEqual(value.trim(), "", `zh.${key} must not be blank`)
  }
}

// ── setLocale drives reactivity ─────────────────────────────────────
{
  const { locale } = useI18n()
  setLocale("zh")
  const zhValue = t("common.enabled")
  setLocale("en")
  assert.equal(locale.value, "en", "setLocale writes the shared reactive ref")
  assert.equal(t("common.enabled"), "Enabled", "t() follows the locale ref")
  assert.notEqual(t("common.enabled"), zhValue)
  setLocale("zh")
  assert.equal(t("common.enabled"), zhValue, "switching back restores the zh copy")
}

// ── installI18n registers $t and tolerates a missing DOM ────────────
{
  setLocale("zh")
  const app = { config: { globalProperties: {} } } as unknown as App
  installI18n(app)
  assert.equal(typeof app.config.globalProperties.$t, "function", "$t is registered globally")
  assert.equal(app.config.globalProperties.$t("common.save"), "保存")
  // No `document` here: the html-lang watcher must not throw, and switching
  // locales afterwards must still be safe.
  assert.equal(typeof document, "undefined", "expected a non-DOM test environment")
  setLocale("en")
  setLocale("zh")
}

console.log("i18n.test: all assertions passed")

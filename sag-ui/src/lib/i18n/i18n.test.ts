import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
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
  // Real dictionary values exercise interpolation now that the `rawFallback`
  // migration valve is gone. Pin the locale so the assertions are about
  // interpolation, not copy.
  try {
    setLocale("en")
    assert.equal(
      t("providers.models.createRouteHint", { provider: "openai", model: "gpt-4o" }),
      "Create route openai/gpt-4o",
      "every placeholder is replaced",
    )
    assert.equal(
      t("analytics.v2.successFailure", { success: 3, failure: 1 }),
      "Success 3 · Failure 1",
      "numbers are stringified",
    )
    // A missing param keeps its literal placeholder while the rest resolves.
    assert.equal(
      t("providers.models.createRouteHint", { provider: "openai" }),
      "Create route openai/{model}",
    )
    // No params at all → the template is returned untouched.
    assert.equal(t("providers.probe.baseUrlLabel"), "{protocol} base URL")
    // A template without placeholders ignores params.
    assert.equal(t("common.save", { name: "x" }), "Save")
  } finally {
    setLocale(DEFAULT_LOCALE)
  }
}

// ── fallback order: active locale → en → the key itself ─────────────
{
  try {
    setLocale("en")
    assert.equal(t("common.save"), "Save")
    setLocale("zh")
    assert.equal(t("common.save"), "保存")

    withZhKeyRemoved("common.save", () => {
      assert.equal(t("common.save"), "Save", "a key missing from zh falls back to en")
    })
    assert.equal(t("common.save"), "保存", "dictionary restored after the probe")

    const missing = "common.not.seeded" as MessageKey
    assert.equal(t(missing), "common.not.seeded", "no fallback left → the key itself")
  } finally {
    setLocale(DEFAULT_LOCALE)
  }
}

// ── missing key warns in dev ────────────────────────────────────────
// `devWarn` is gated on `import.meta.env.DEV`, which Vite injects at build
// time and the plain node runner never defines. The probe loads a fresh copy
// of the module through a `module.registerHooks` loader that prepends
// `import.meta.env = { DEV: true }`, so the dev-only branch is genuinely
// exercised instead of being stubbed away.
function probeMissingKeyWarn(): { value: string; warns: string[] } {
  const moduleUrl = new URL("./index.ts", import.meta.url).href
  const script = `
import { registerHooks } from "node:module"
import { readFileSync } from "node:fs"
const target = ${JSON.stringify(moduleUrl)}
registerHooks({
  load(url, context, nextLoad) {
    if (url === target) {
      const source = readFileSync(new URL(url), "utf8")
      return {
        format: "module-typescript",
        source: "import.meta.env = { DEV: true };\\n" + source,
        shortCircuit: true,
      }
    }
    return nextLoad(url, context)
  },
})
const mod = await import(target)
const warns = []
const original = console.warn
console.warn = (...args) => warns.push(args.map(String).join(" "))
let value
try {
  value = mod.t("common.not.seeded")
} catch (error) {
  console.error("probe threw: " + error.message)
  process.exit(3)
} finally {
  console.warn = original
}
process.stdout.write("PROBE_RESULT:" + JSON.stringify({ value, warns }) + "\\n")
`
  const stdout = execFileSync(
    process.execPath,
    ["--experimental-strip-types", "--input-type=module", "-e", script],
    { encoding: "utf8", timeout: 15_000, env: { ...process.env, NODE_NO_WARNINGS: "1" } },
  )
  const marker = "PROBE_RESULT:"
  const line = stdout.split("\n").find((l) => l.startsWith(marker))
  if (line === undefined) throw new Error(`probe produced no result marker:\n${stdout}`)
  return JSON.parse(line.slice(marker.length)) as { value: string; warns: string[] }
}

{
  const result = probeMissingKeyWarn()
  assert.equal(result.value, "common.not.seeded", "missing key returns the key itself")
  assert.ok(
    result.warns.some((w) => w.includes("missing key") && w.includes("common.not.seeded")),
    `expected a dev warning, got ${JSON.stringify(result.warns)}`,
  )
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
  try {
    const { locale } = useI18n()
    setLocale("zh")
    const zhValue = t("common.enabled")
    setLocale("en")
    assert.equal(locale.value, "en", "setLocale writes the shared reactive ref")
    assert.equal(t("common.enabled"), "Enabled", "t() follows the locale ref")
    assert.notEqual(t("common.enabled"), zhValue)
    setLocale("zh")
    assert.equal(t("common.enabled"), zhValue, "switching back restores the zh copy")
  } finally {
    setLocale(DEFAULT_LOCALE)
  }
}

// ── installI18n registers $t and tolerates a missing DOM ────────────
{
  try {
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
  } finally {
    setLocale(DEFAULT_LOCALE)
  }
}

// ── corrupted persisted locale is coerced, never crashes ────────────
// The module caches its locale ref on first use, and `@vueuse/shared` freezes
// `isClient` when it is first imported — both already happened above without a
// DOM. Seeded-storage scenarios therefore run in a fresh child process where
// the fake DOM globals exist *before* the module loads.
function probeStoredLocale(seed: string | null): { locale: unknown; value: string } {
  const moduleUrl = new URL("./index.ts", import.meta.url).href
  const script = `
class FakeStorage {
  #map = new Map()
  get length() { return this.#map.size }
  getItem(key) { return this.#map.has(key) ? this.#map.get(key) : null }
  setItem(key, value) { this.#map.set(key, String(value)) }
  removeItem(key) { this.#map.delete(key) }
  clear() { this.#map.clear() }
  key(index) { return [...this.#map.keys()][index] ?? null }
}
globalThis.Storage = FakeStorage
globalThis.localStorage = new FakeStorage()
${seed === null ? "" : `localStorage.setItem(${JSON.stringify(LOCALE_STORAGE_KEY)}, ${JSON.stringify(seed)})`}
globalThis.window = globalThis
globalThis.addEventListener = () => {}
globalThis.removeEventListener = () => {}
globalThis.dispatchEvent = () => {}
globalThis.StorageEvent = class StorageEvent {
  constructor(type, init) { this.type = type; Object.assign(this, init) }
}
globalThis.document = {
  documentElement: { lang: "" },
  createElement: () => ({ innerHTML: "", content: { firstChild: null }, setAttribute() {}, style: {} }),
  createTextNode: () => ({}),
  querySelector: () => null,
}
const mod = await import(${JSON.stringify(moduleUrl)})
const { locale } = mod.useI18n()
let value
try {
  value = mod.t("common.save")
} catch (error) {
  console.error("probe threw: " + error.message)
  process.exit(3)
}
process.stdout.write("PROBE_RESULT:" + JSON.stringify({ locale: locale.value, value }) + "\\n")
`
  const stdout = execFileSync(
    process.execPath,
    ["--experimental-strip-types", "--input-type=module", "-e", script],
    { encoding: "utf8", timeout: 15_000, env: { ...process.env, NODE_NO_WARNINGS: "1" } },
  )
  // The child payload is self-delimiting: unrelated stdout noise (warnings,
  // future debug logs) is ignored instead of breaking `JSON.parse`.
  const marker = "PROBE_RESULT:"
  const line = stdout.split("\n").find((l) => l.startsWith(marker))
  if (line === undefined) throw new Error(`probe produced no result marker:\n${stdout}`)
  return JSON.parse(line.slice(marker.length)) as { locale: unknown; value: string }
}

{
  const corrupted: (string | null)[] = ["fr", "", "FR", "zh-CN", null, '{"a":1}']
  for (const seed of corrupted) {
    const result = probeStoredLocale(seed)
    const label = `seed ${JSON.stringify(seed)}`
    assert.equal(result.locale, DEFAULT_LOCALE, `${label} is coerced to the default locale`)
    assert.equal(result.value, "保存", `${label} translates without throwing`)
  }
  // A valid persisted locale still survives untouched.
  assert.deepEqual(probeStoredLocale("en"), { locale: "en", value: "Save" })
  assert.deepEqual(probeStoredLocale("zh"), { locale: "zh", value: "保存" })
}

console.log("i18n.test: all assertions passed")

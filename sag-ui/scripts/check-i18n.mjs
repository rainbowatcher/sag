/**
 * Anti-regression checks for the DIY i18n layer (P4).
 *
 * Run with `node --experimental-strip-types scripts/check-i18n.mjs`
 * (or `just ui-i18n-check`). No dependencies: the dictionaries are plain TS
 * objects loaded through the same file-URL import the subprocess test uses.
 *
 * Rules enforced — any failure prints the offending key/value/file and exits 1:
 *   a. Key parity: `en` and `zh` expose exactly the same key set.
 *   b. Placeholder parity: per key, the sorted `{param}` token lists are equal.
 *   c. Stray braces: every value has balanced braces and every `{` opens a
 *      valid `{name}` placeholder.
 *   d. CJK scan: no Chinese characters in `src/**\/*.{ts,vue}`.
 *      EXACT RULE: every file under `src/lib/i18n/` is skipped entirely
 *      (`zh.ts` is Chinese by design; `i18n.test.ts` asserts Chinese values).
 *      `zh.ts` itself is still validated by rules a–c because it is imported.
 *      For every other file, a line whose trimmed text starts with `//`, `/*`,
 *      `*` or `<!--` is treated as a comment and skipped; any remaining
 *      character in the Han / CJK-punctuation / fullwidth ranges
 *      (U+3400–U+4DBF, U+4E00–U+9FFF, U+F900–U+FAFF, U+3000–U+303F,
 *      U+FF00–U+FFEF) fails the check.
 *   e. Dead keys — REPORT ONLY, never fails: keys defined in `en` that no
 *      `t("key"` / `t('key'` / `$t("key"` / `$t('key'` occurrence in `src/**`
 *      references. P3 may have minted a few unused keys; the count and list are
 *      printed so they stay visible.
 */
import { readdirSync, readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

const srcRoot = new URL("../src/", import.meta.url)
const enUrl = new URL("../src/lib/i18n/en.ts", import.meta.url).href
const zhUrl = new URL("../src/lib/i18n/zh.ts", import.meta.url).href

const { en } = await import(enUrl)
const { zh } = await import(zhUrl)

const CJK_PATTERN = /[\u3400-\u4DBF\u4E00-\u9FFF\uF900-\uFAFF\u3000-\u303F\uFF00-\uFFEF]/
const PLACEHOLDER_PATTERN = /\{(\w+)\}/g
const REFERENCE_PATTERN = /(?<![\w$.])\$?t\(\s*["']([^"']+)["']/g

const failures = []
const fail = (message) => failures.push(message)

function walk(dir) {
  const out = []
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = new URL(entry.name + (entry.isDirectory() ? "/" : ""), dir)
    if (entry.isDirectory()) out.push(...walk(path))
    else if (/\.(ts|vue)$/.test(entry.name)) out.push(path)
  }
  return out
}

const isCommentLine = (line) => {
  const trimmed = line.trim()
  return (
    trimmed.startsWith("//") ||
    trimmed.startsWith("/*") ||
    trimmed.startsWith("*") ||
    trimmed.startsWith("<!--")
  )
}

// ── (a) key parity ──────────────────────────────────────────────────
const enKeys = Object.keys(en).sort()
const zhKeys = Object.keys(zh).sort()
const enKeySet = new Set(enKeys)
const zhKeySet = new Set(zhKeys)
for (const key of enKeys) {
  if (!zhKeySet.has(key)) fail(`(a) key parity: zh is missing "${key}"`)
}
for (const key of zhKeys) {
  if (!enKeySet.has(key)) fail(`(a) key parity: zh has extra key "${key}"`)
}

// ── (b) placeholder parity + (c) stray braces ───────────────────────
for (const key of enKeys) {
  const enValue = en[key]
  const zhValue = zh[key]
  if (typeof enValue !== "string" || typeof zhValue !== "string") continue
  const tokens = (value) => [...value.matchAll(PLACEHOLDER_PATTERN)].map((m) => m[1]).sort()
  const enTokens = tokens(enValue)
  const zhTokens = tokens(zhValue)
  if (enTokens.join("\u0000") !== zhTokens.join("\u0000")) {
    fail(
      `(b) placeholder parity: "${key}" en={${enTokens}} zh={${zhTokens}} ` +
        `(en ${JSON.stringify(enValue)} / zh ${JSON.stringify(zhValue)})`,
    )
  }
  for (const [locale, value] of [
    ["en", enValue],
    ["zh", zhValue],
  ]) {
    const opens = (value.match(/\{/g) ?? []).length
    const closes = (value.match(/\}/g) ?? []).length
    if (opens !== closes) {
      fail(`(c) stray braces: ${locale}."${key}" has ${opens} "{" and ${closes} "}"`)
      continue
    }
    const stripped = value.replace(PLACEHOLDER_PATTERN, "")
    if (stripped.includes("{") || stripped.includes("}")) {
      fail(`(c) stray braces: ${locale}."${key}" = ${JSON.stringify(value)}`)
    }
  }
}

// ── (d) CJK-literal scan + (e) dead-key report ──────────────────────
const referenced = new Set()
const files = walk(srcRoot)
for (const file of files) {
  const path = fileURLToPath(file)
  const content = readFileSync(file, "utf8")
  // Reference scan is content-based: `t(` and its key may sit on different
  // lines (the `\s*` in the pattern spans the newline). It is grep-shaped, so
  // a key named in a comment still counts as referenced.
  for (const match of content.matchAll(REFERENCE_PATTERN)) referenced.add(match[1])
  if (path.includes("/lib/i18n/")) continue
  content.split("\n").forEach((line, index) => {
    if (!isCommentLine(line) && CJK_PATTERN.test(line)) {
      fail(`(d) CJK literal: ${path}:${index + 1}: ${line.trim()}`)
    }
  })
}

const deadKeys = enKeys.filter((key) => !referenced.has(key))

// ── report ──────────────────────────────────────────────────────────
if (failures.length > 0) {
  console.error(`✗ i18n check failed with ${failures.length} problem(s):`)
  for (const message of failures) console.error(`  - ${message}`)
  process.exit(1)
}

console.log(
  `✓ i18n check passed: ${enKeys.length} keys, en/zh parity, placeholders and braces clean, no CJK literals outside src/lib/i18n/`,
)
if (deadKeys.length > 0) {
  console.log(`ℹ dead-key report (${deadKeys.length} key(s) defined but never referenced):`)
  for (const key of deadKeys) console.log(`  - ${key}`)
} else {
  console.log("ℹ dead-key report: none")
}

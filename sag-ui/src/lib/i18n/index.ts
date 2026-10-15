/**
 * Lightweight DIY i18n core (no vue-i18n): flat dot-separated keys, named
 * `{param}` interpolation, EN/ZH locales with `zh` as the default.
 *
 * Relative imports carry explicit `.ts` extensions because this module is also
 * loaded by the node test runner (`node --experimental-strip-types`), which
 * resolves specifiers verbatim and cannot resolve extensionless imports.
 */
import { useLocalStorage } from "@vueuse/core"
import type { App, Ref } from "vue"
import { watchEffect } from "vue"
import { en } from "./en.ts"
import { zh } from "./zh.ts"

export type Locale = "en" | "zh"
/** Every message key, derived from the canonical `en` dictionary. */
export type MessageKey = keyof typeof en

/** localStorage key holding the persisted UI language. */
export const LOCALE_STORAGE_KEY = "sag-ui-lang"
/** Default UI language: Chinese unless the user switches. */
export const DEFAULT_LOCALE: Locale = "zh"

const messages: Record<Locale, Record<string, string>> = { en, zh }

const env = (import.meta as ImportMeta & { env?: { DEV?: boolean } }).env
const isDev = env?.DEV === true

function devWarn(message: string): void {
  if (isDev) console.warn(`[i18n] ${message}`)
}

// `useLocalStorage` touches `window` when it is available, so the ref is built
// lazily on first use rather than at module load — unit tests import this
// module in a plain node environment (no `localStorage`).
let localeRef: Ref<Locale> | null = null

function getLocale(): Ref<Locale> {
  localeRef ??= useLocalStorage<Locale>(LOCALE_STORAGE_KEY, DEFAULT_LOCALE)
  return localeRef
}

const PARAM_PATTERN = /\{(\w+)\}/g

/** Replace every `{name}` with `String(params.name)`; unknown names stay literal. */
function interpolate(template: string, params?: Record<string, string | number>): string {
  if (!params) return template
  return template.replace(PARAM_PATTERN, (placeholder, name: string) => {
    const value = params[name]
    if (value === undefined) {
      devWarn(`missing param "${name}" for "${template}"`)
      return placeholder
    }
    return String(value)
  })
}

/**
 * Translate `key` into the active locale.
 *
 * Resolution order: active locale → `en` → `rawFallback` → `key`. The
 * `rawFallback` valve lets not-yet-migrated call sites pass their current
 * hardcoded string instead of showing a raw key.
 */
export function t(
  key: MessageKey,
  params?: Record<string, string | number>,
  rawFallback?: string,
): string {
  const active = getLocale().value
  let template: string | undefined = messages[active][key]
  if (template === undefined) {
    const fallback: string | undefined = en[key] ?? rawFallback
    template = fallback ?? key
    devWarn(`missing key "${key}" for locale "${active}", falling back to "${template}"`)
  }
  return interpolate(template, params)
}

/** Switch the active language (persisted through `useLocalStorage`). */
export function setLocale(locale: Locale): void {
  getLocale().value = locale
}

/** Composable access to the translator and the reactive locale ref. */
export function useI18n(): { t: typeof t; locale: Ref<Locale>; setLocale: typeof setLocale } {
  return { t, locale: getLocale(), setLocale }
}

/** Install `$t` globally and keep `<html lang>` in sync with the locale. */
export function installI18n(app: App): void {
  app.config.globalProperties.$t = t
  watchEffect(() => {
    if (typeof document === "undefined") return
    document.documentElement.lang = getLocale().value === "zh" ? "zh-CN" : "en"
  })
}

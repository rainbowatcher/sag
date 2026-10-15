import assert from "node:assert/strict"
import { FormApi } from "@tanstack/vue-form"
import { z } from "zod"
import { en } from "./en.ts"
import { DEFAULT_LOCALE, setLocale, t } from "./index.ts"
import { zh } from "./zh.ts"

// Locale-switch regression for TanStack form validation messages (P1/P2
// review follow-up, Minor-1). The production forms declare `validators` as a
// getter so the Zod schema — and therefore its `t()` message — is rebuilt on
// every validation run. This probe reproduces that shape against the real
// `FormApi` and asserts the message follows `setLocale()`.
//
// Import note: `@tanstack/form-core` is not a direct dependency of sag-ui
// (pnpm links it only under `vue-form`), so `import ... from
// "@tanstack/form-core"` throws ERR_MODULE_NOT_FOUND under the node runner.
// `@tanstack/vue-form` re-exports the same `FormApi` and resolves cleanly; it
// runs headlessly (no DOM).
//
// Why `form.validate("submit")` and not `form.handleSubmit()`: form-core's
// `handleSubmit` short-circuits at `validateAllFields("submit")` once
// `isFieldsValid` is false, so a second submit after a failure never reaches
// the form-level validator. `validate("submit")` is the exact call that runs
// `validators.onSubmit`, so it is what proves the getter re-reads `t()` after a
// locale switch.
const form = new FormApi({
  defaultValues: { name: "" },
  // Getter (not a plain schema): form-core re-reads `options.validators` on
  // every run, so a locale switch is picked up without rebuilding the form.
  get validators() {
    return {
      onSubmit: z.object({ name: z.string().min(1, t("providers.form.nameRequired")) }),
    }
  },
})

/** The message form-core surfaced for the invalid `name` field. */
function nameMessage(): string {
  const issues = form.state.errorMap.onSubmit as unknown as
    | Record<string, { message?: string }[]>
    | undefined
  return issues?.name?.[0]?.message ?? ""
}

try {
  setLocale("zh")
  await form.validate("submit")
  assert.equal(
    nameMessage(),
    zh["providers.form.nameRequired"],
    "zh locale renders the zh validation message",
  )

  setLocale("en")
  await form.validate("submit")
  assert.equal(
    nameMessage(),
    en["providers.form.nameRequired"],
    "after setLocale('en') the validation message flips to English",
  )

  setLocale("zh")
  await form.validate("submit")
  assert.equal(nameMessage(), zh["providers.form.nameRequired"], "and flips back to zh")
} finally {
  setLocale(DEFAULT_LOCALE)
}

console.log("i18n-form.test: all assertions passed")

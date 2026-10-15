/**
 * Canonical UI dictionary (English).
 *
 * This object is the single source of truth for message keys: `MessageKey` is
 * derived from it, and every other locale must cover the exact same key set.
 * Keys are flat and dot-separated (`<module>.<thing>`) — no nesting, no
 * plurals, no pipes.
 */
export const en = {
  // ── Shared vocabulary ───────────────────────────────────────────
  "common.save": "Save",
  "common.cancel": "Cancel",
  "common.confirm": "Confirm",
  "common.delete": "Delete",
  "common.edit": "Edit",
  "common.close": "Close",
  "common.refresh": "Refresh",
  "common.loading": "Loading...",
  "common.retry": "Retry",
  "common.expired": "Expired",
  "common.enabled": "Enabled",
  "common.disabled": "Disabled",
  "common.actions": "Actions",
  "common.status": "Status",
  "common.name": "Name",
  "common.description": "Description",

  // ── Settings › Language ─────────────────────────────────────────
  "settings.language.tab": "Language",
  "settings.language.title": "Language",
  "settings.language.description":
    "Choose the display language of the admin dashboard. The preference is stored in this browser.",
  "settings.language.label": "Interface language",
  "settings.language.optionEnglish": "English",
  "settings.language.optionChinese": "中文",

  // ── Circuit breakers ────────────────────────────────────────────
  "circuitBreaker.recovered":
    "Recovered {provider}/{model} — the next request will go straight upstream",
  "circuitBreaker.restoreFailed": "Failed to recover",
  "circuitBreaker.forceCloseTitle":
    "Force-recover this target; the next request will go straight upstream",
  "circuitBreaker.restore": "Recover",
  "circuitBreaker.cooldownElapsed": "Recoverable now",
  "circuitBreaker.probeElapsed": "Probeable now",

  // ── Providers ───────────────────────────────────────────────────
  "providers.addForm.timeoutPlaceholder": "600 (default)",
  "providers.advanced.bodyOverrideWarningTitle": "Note",
  "providers.advanced.bodyOverrideWarningLead": "Body Overrides apply to ",
  "providers.advanced.bodyOverrideWarningAllModels": "all models",
  "providers.advanced.bodyOverrideWarningAfterAllModels":
    " of this provider. To set different parameters for a specific model, go to ",
  "providers.advanced.bodyOverrideWarningApisTab": "APIs Tab",
  "providers.advanced.bodyOverrideWarningAfterApisTab":
    " in the corresponding API section's Advanced and configure ",
  "providers.advanced.bodyOverrideWarningModelOverrides": "Model Overrides",
  "providers.advanced.bodyOverrideWarningAfterModelOverrides": ", avoiding duplicating ",
  "providers.advanced.bodyOverrideWarningEnd": " across the targets of multiple routes.",

  // ── Analytics ───────────────────────────────────────────────────
  "analytics.approxPercentilesTooltip":
    "Historical range estimated from the 1h Rollup histogram (≈1% error), not exact percentiles",
  "analytics.cacheUnknownTooltip":
    "Number of successful samples that did not report cached_tokens (unknown ≠ miss, excluded from the hit-rate denominator)",
  "analytics.overallOnlyMetric":
    "This metric is only available on the overall line, with no per-group breakdown",
} as const

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
  "common.provider": "Provider",
  "common.model": "Model",
  "common.yes": "Yes",
  "common.no": "No",

  // ── Navigation ──────────────────────────────────────────────────
  "nav.dashboard": "Dashboard",
  "nav.providers": "Providers",
  "nav.routes": "Routes",
  "nav.analytics": "Analytics",
  "nav.logs": "Logs",
  "nav.settings": "Settings",

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

  // ── Providers: toasts ───────────────────────────────────────────
  "providers.toast.created": 'Provider "{name}" created',
  "providers.toast.updated": 'Provider "{name}" updated',
  "providers.toast.deleted": 'Provider "{name}" deleted',
  "providers.toast.enabled": 'Provider "{name}" enabled',
  "providers.toast.disabled": 'Provider "{name}" disabled',
  "providers.toast.copyFailed": "Failed to copy provider",

  // ── Providers: form validation ──────────────────────────────────
  "providers.form.nameRequired": "Name is required",
  "providers.form.apiKeyRequired": "At least one API key is required",
  "providers.form.bodyOverrideInvalidJson": 'Body override "{key}": invalid JSON value',

  // ── Providers: list table ───────────────────────────────────────
  "providerList.columns.apis": "APIs",
  "providerList.columns.keys": "Keys",
  "providerList.columns.cb": "CB",
  "providerList.allTargetsHealthy": "All targets healthy",
  "providerList.breakerOpenGroup": "Open ({count}):",
  "providerList.breakerHalfOpenGroup": "Half-Open ({count}):",
  "providerList.enableProvider": "Enable provider",
  "providerList.disableProvider": "Disable provider",
  "providerList.editProvider": "Edit provider",
  "providerList.copyProvider": "Copy provider",
  "providerList.deleteProvider": "Delete provider",
  "providerList.cancelDelete": "Cancel delete",
  "providerList.fallbackAuto": "fallback: auto",
  "providerList.fallbackNamed": "fallback: {name}",

  // ── Routes: toasts ──────────────────────────────────────────────
  "routes.toast.created": 'Route "{name}" created',
  "routes.toast.updated": 'Route "{name}" updated',
  "routes.toast.deleted": 'Route "{name}" deleted',
  "routes.toast.createFailed": "Failed to create route",
  "routes.toast.fillFirstReset":
    'Reset fill-first order for "{model}", will re-shuffle on next request',
  "routes.toast.pinCleared": 'Cleared sticky pin for "{model}", will revert to priority routing',
  "routes.toast.clearActiveFailed": "Failed to clear active target",

  // ── Routes: form validation ─────────────────────────────────────
  "routes.form.apiEndpointRequired": "API endpoint is required for image pipeline",
  "routes.form.targetRequired": "At least one target is required",
  "routes.form.targetProviderRequired": "Each target must have a provider selected",
  "routes.form.modelNameRequired":
    "Please provide model_name or fill real_model on the first target",
  "routes.form.targetExtra": "Target {name}: {error}",

  // ── Routes: list table ──────────────────────────────────────────
  "routeList.columns.modelName": "Model Name",
  "routeList.columns.pipeline": "Pipeline",
  "routeList.columns.strategy": "Strategy",
  "routeList.columns.targets": "Targets",
  "routeList.columns.activeTarget": "Active Target",
  "routeList.allTargetsUnavailable": "All targets are unavailable",
  "routeList.targetsValid": "{valid}/{total} targets valid",
  "routeList.targetMissingProvider": 'Target #{index}: Provider "{provider}" not found',
  "routeList.targetProviderDisabled": 'Target #{index}: Provider "{provider}" is disabled',
  "routeList.targetModelDisabled":
    'Target #{index}: Model "{model}" is not enabled for provider "{provider}"',
  "routeList.tooltip.orderAge": "Order age",
  "routeList.tooltip.pinAge": "Pin age",
  "routeList.tooltip.action": "Action",
  "routeList.fillFirstTarget": "Fill-first target",
  "routeList.clickToReset": "Click to reset",
  "routeList.clickToClear": "Click to clear",
  "routeList.editRoute": "Edit route",
  "routeList.deleteRoute": "Delete route",

  // ── Analytics › V2 panel ────────────────────────────────────────
  "analytics.v2.columns.actualModel": "Actual Model",
  "analytics.v2.columns.attempts": "Attempts",
  "analytics.v2.columns.success": "Success",
  "analytics.v2.columns.failure": "Failure",
  "analytics.v2.columns.successRate": "Success Rate",
  "analytics.v2.columns.avgE2e": "Avg E2E",
  "analytics.v2.columns.p95E2e": "P95 E2E",
  "analytics.v2.columns.tokensIn": "Tokens In",
  "analytics.v2.columns.tokensOut": "Tokens Out",
  "analytics.v2.columns.cacheHit": "Cache Hit",
  "analytics.v2.columns.cacheUnknown": "Cache Unknown",
  "analytics.v2.columns.routeName": "Route Name",
  "analytics.v2.columns.requests": "Requests",
  "analytics.v2.columns.modelFailure": "Model Failure",
  "analytics.v2.others": "others",
  "analytics.v2.card.e2e": "E2E",
  "analytics.v2.card.ttft": "TTFT",
  "analytics.v2.card.upstream": "Upstream",

  // ── Settings: toasts ────────────────────────────────────────────
  "settings.toast.circuitBreakerSaved": "Circuit breaker settings saved",
  "settings.toast.backoffSaved": "Backoff settings saved",
  "settings.toast.strategyTimersSaved": "Strategy timers saved",
  "settings.toast.proxySaved": "Proxy settings saved",
  "settings.toast.proxyCleared": "Proxy settings cleared",
  "settings.toast.apiKeysSaved": "API keys saved",
  "settings.toast.authDisabled": "Authentication disabled",

  // ── Settings: proxy / API-key validation ────────────────────────
  "settings.proxy.invalidUrl": "Invalid URL. Use the form http://host:port or https://host:port.",
  "settings.proxy.httpOnly": "Only HTTP/HTTPS proxy URLs are supported (SOCKS is not supported).",
  "settings.apiKeys.emptyOrWhitespace": "API keys must not be empty or whitespace-only.",
  "settings.apiKeys.emptyRows": "Remove empty key rows before saving, or fill them in.",

  // ── Settings: read-only info rows ───────────────────────────────
  "settings.info.host": "Host",
  "settings.info.port": "Port",
  "settings.info.globalProxy": "Global proxy",
  "settings.info.direct": "Direct (no proxy)",
  "settings.info.level": "Level",
  "settings.info.format": "Format",
  "settings.info.enabled": "Enabled",
  "settings.info.storageDsn": "Storage DSN",

  // ── Settings: numeric field validation ──────────────────────────
  "settings.form.secsMin1": "{label} must be a whole number of seconds (≥ 1).",
  "settings.form.secsMin0": "{label} must be a whole number of seconds (≥ 0).",
  "settings.form.secsMax": "{label} must not exceed {max} seconds (one year).",
  "settings.circuitBreaker.cooldownQuota": "Quota-exhausted cooldown",
  "settings.circuitBreaker.cooldownRate": "Rate-limited cooldown",
  "settings.circuitBreaker.cooldownGeneric": "Generic cooldown",
  "settings.circuitBreaker.probeInterval": "Probe interval",
  "settings.strategyTimers.fillFirstTtl": "Fill-first cache TTL",
  "settings.strategyTimers.stickyReset": "Sticky failover reset",
  "settings.backoff.retriesMin": "Retries must be at least 1.",
  "settings.backoff.baseNonNegative": "Base delay must be a non-negative number.",
  "settings.backoff.maxNonNegative": "Max delay must be a non-negative number.",
  "settings.backoff.baseExceedsMax": "Base delay must not exceed max delay.",
} as const

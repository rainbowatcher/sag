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
  "common.value": "Value",
  "common.saving": "Saving...",
  "common.default": "Default",
  "common.inherit": "Inherit",

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
  "circuitBreaker.title": "Circuit Breakers",
  "circuitBreaker.tripped": "{count} tripped",
  "circuitBreaker.refreshTitle": "Refresh circuit breaker state",
  "circuitBreaker.allHealthy": "All targets healthy — no circuits tripped.",
  "circuitBreaker.stateOpen": "Open",
  "circuitBreaker.stateHalfOpen": "Half-Open",
  "circuitBreaker.probing": "probing",
  "circuitBreaker.probingTitle": "A probe request is currently in flight",
  "circuitBreaker.cooldown": "Cooldown:",
  "circuitBreaker.nextProbe": "Next probe:",
  "circuitBreaker.probeReady": "Probe ready",

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

  // ── Providers: form shell ───────────────────────────────────────
  "providers.tabs.basic": "Basic",
  "providers.tabs.apis": "APIs",
  "providers.tabs.models": "Models",
  "providers.tabs.advanced": "Advanced",
  "providers.addForm.title": "New Provider",
  "providers.addForm.description": "Configure a new upstream provider connection",
  "providers.addForm.adding": "Adding...",
  "providers.addForm.createProvider": "Create Provider",
  "providers.editForm.description": "Edit provider configuration",
  "providers.form.nameLabel": "Name *",
  "providers.form.namePlaceholder": "my-openai",
  "providers.form.timeoutLabel": "Timeout (secs)",
  "providers.form.keyStrategy": "Key Strategy",
  "providers.form.fillFirst": "Fill First",
  "providers.form.roundRobin": "Round Robin",
  "providers.form.proxyMode": "Proxy Mode",
  "providers.form.proxyModeHelp":
    "How this provider resolves the upstream proxy. Inherit follows the gateway-level proxy URL.",

  // ── Providers: list ─────────────────────────────────────────────
  "providers.list.searchPlaceholder": "Search providers...",
  "providers.list.filterAllApis": "All API sections",
  "providers.list.sortName": "Name (A-Z)",
  "providers.list.addProvider": "Add Provider",
  "providers.list.noMatch": "No providers match the current filters.",
  "providers.list.empty": 'No providers configured. Click "Add Provider" to get started.',
  "providers.list.deleteConfirmPrefix": "Delete provider ",
  "providers.list.deleteConfirmSuffix": "?",
  "providers.list.deleting": "Deleting…",
  "providers.list.copyTitle": "Copy Provider",
  "providers.list.copyDescLead": "Create a new provider by copying ",
  "providers.list.copyDescTail": ". All settings and API keys are reused.",
  "providers.list.copyNameLabel": "Provider Name",
  "providers.list.copyNamePlaceholder": "e.g. my-openai-copy",
  "providers.list.copyExists": 'Provider "{name}" already exists.',
  "providers.list.copying": "Copying…",
  "providers.list.copyAndEdit": "Copy & Edit",

  // ── Providers: advanced tab ─────────────────────────────────────
  "providers.advanced.introLead":
    "Provider-level defaults below apply to every API section. A field declared inside an ",
  "providers.advanced.introApisTab": "APIs",
  "providers.advanced.introMid": " section ",
  "providers.advanced.introFullyReplaces": "fully replaces",
  "providers.advanced.introTail":
    " the default of the same name for that section (maps are not merged, arrays not unioned).",
  "providers.advanced.headersTitle": "Headers",
  "providers.advanced.headersDescription":
    "Key-value pairs of custom headers (values are plain text).",
  "providers.advanced.headerKeyPlaceholder": "Header-Name",
  "providers.advanced.bodyOverridesTitle": "Body Overrides",
  "providers.advanced.bodyOverridesDescription":
    'Key-value pairs overridden in the upstream JSON body. Values are parsed as JSON (e.g. <code>4096</code>, <code>true</code>, <code>"text"</code>).',
  "providers.advanced.stripFieldsTitle": "Strip Fields",
  "providers.advanced.stripFieldsDescription":
    "Top-level fields to remove from the upstream JSON body before sending. Applied last, after all overrides. Use for fields the upstream rejects (e.g. <code>chat_template_kwargs</code> for pydantic <code>extra=forbid</code>).",
  "providers.advanced.addField": "Add Field",
  "providers.advanced.roleMappingsTitle": "Role Mappings",
  "providers.advanced.roleMappingsDescriptionDefault":
    "Maps request role names to roles the upstream supports (case-sensitive). Example: developer → system.",
  "providers.advanced.roleMappingsDescriptionEdit":
    "Map request role names to role names supported by the upstream (case-sensitive). Example: developer → system.",

  // ── Providers: APIs editor ──────────────────────────────────────
  "providers.apis.addSectionPlaceholder": "Add API section…",
  "providers.apis.addSection": "Add Section",
  "providers.apis.fallbackAria": "Fallback: {kind}",
  "providers.apis.removeSection": "Remove API section",
  "providers.apis.baseUrlLabel": "Base URL *",
  "providers.apis.modelsEndpointLabel": "Models Endpoint",
  "providers.apis.modelOverridesTitle": "Model Overrides",
  "providers.apis.cacheControl": "Cache Control",
  "providers.apis.cacheInherit": "Inherit default",
  "providers.apis.chunkTimeout": "Chunk Timeout (secs)",
  "providers.apis.ttfbTimeout": "TTFB Timeout (secs)",
  "providers.apis.atLeastOneSection": "At least one API section is required.",
  "providers.apis.selectFallback": "Select a fallback protocol (radio on a protocol API section).",
  "providers.apis.protocolProbeSupport": "Protocol Probe Support",
  "providers.apis.validate.atLeastOneSection":
    "At least one API section is required (e.g. chat_completions)",
  "providers.apis.validate.sectionPrefix": 'API section "{kind}": {error}',
  "providers.apis.validate.baseUrlRequired": 'API section "{kind}": base_url is required',
  "providers.apis.validate.modelOverrideNotObject":
    'Model override "{key}": value must be a JSON object',
  "providers.apis.validate.modelOverrideInvalidJson": 'Model override "{key}": invalid JSON value',
  "providers.apis.validate.timeoutNonNegative":
    'API section "{kind}": timeout must be a non-negative number',
  "providers.apis.validate.chunkTimeoutNonNegative":
    'API section "{kind}": chunk timeout must be a non-negative number',
  "providers.apis.validate.ttfbTimeoutNonNegative":
    'API section "{kind}": TTFB timeout must be a non-negative number',
  "providers.apis.validate.fallbackRequired":
    "Fallback protocol is required when multiple protocol sections are declared",

  // ── Providers: protocol probe ───────────────────────────────────
  "providers.probe.description":
    "Read-only probe: one minimal request per protocol × key. Nothing is saved until you backfill and store the provider.",
  "providers.probe.applyAllTitle": "Apply this base URL to all protocols",
  "providers.probe.applyAll": "Apply to all",
  "providers.probe.baseUrlLabel": "{protocol} base URL",
  "providers.probe.modelLabel": "Model *",
  "providers.probe.probing": "Probing…",
  "providers.probe.probe": "Probe",
  "providers.probe.backfillOne": "Backfill 1 protocol",
  "providers.probe.backfillMany": "Backfill {count} protocols",
  "providers.probe.modelHintLead": "Model: prefer a non-reasoning model. Use ",
  "providers.probe.modelHintTail": " to share one base URL across protocols.",
  "providers.probe.addKeyFirst": "Add at least one API key first.",
  "providers.probe.resultsTitle": "Probe results",
  "providers.probe.columns.protocol": "Protocol",
  "providers.probe.columns.verdict": "Verdict",
  "providers.probe.columns.key": "Key",
  "providers.probe.columns.message": "Message",
  "providers.probe.backfillHint":
    "Backfill fills matching sections only (supported / auth failed / partial).",
  "providers.probe.protocolChat": "Chat Completions",
  "providers.probe.protocolResponses": "Responses",
  "providers.probe.protocolAnthropic": "Anthropic Messages",
  "providers.probe.verdictSupported": "supported",
  "providers.probe.verdictAuthFailed": "auth failed",
  "providers.probe.verdictUncertain": "uncertain",
  "providers.probe.verdictUnsupported": "unsupported",
  "providers.probe.verdictPartial": "partial",
  "providers.probe.verdictError": "error",

  // ── Providers: model manager ────────────────────────────────────
  "providers.models.noRestriction":
    "No models listed means all models are allowed (no restriction).",
  "providers.models.clickToTest": "Click to test",
  "providers.models.createRouteTitle": "Create route",
  "providers.models.createRouteHint": "Create route {provider}/{model}",
  "providers.models.selectPlaceholder": "Select models...",
  "providers.models.searchPlaceholder": "Search models...",
  "providers.models.emptyText": "No models found. Click Refresh Models.",
  "providers.models.customPlaceholder": "Or type a model name...",
  "providers.models.add": "Add",
  "providers.models.refresh": "Refresh Models",
  "providers.models.loadingModels": "Loading models...",
  "providers.models.createRouteDialogTitle": "Create Route",
  "providers.models.createRouteDescLead": "Create a single-target route for ",
  "providers.models.createRouteDescTail": ".",
  "providers.models.routeNameLabel": "Route Name",
  "providers.models.routeNamePlaceholder": "e.g. volc/deepseek-v4-flash",
  "providers.models.routeExists": 'Route "{name}" already exists.',
  "providers.models.creating": "Creating…",

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
  "routeList.strategy.stickyFailover": "sticky-failover",
  "routeList.strategy.fillFirst": "fill-first",
  "routeList.strategy.roundRobin": "round-robin",
  "routeList.noMatch": "No routes match the current filters.",
  "routeList.resetFillFirstTitle": "Reset Fill-First Order",
  "routeList.clearActiveTitle": "Clear Active Target",
  "routeList.resetFillFirstDescLead": "Reset the cached fill-first order for ",
  "routeList.resetFillFirstDescTail":
    "? The next request will re-shuffle and may route to a different target.",
  "routeList.clearActiveDescLead": "Clear the active target ",
  "routeList.clearActiveDescTail": "? Traffic will revert to priority routing.",
  "routeList.clearing": "Clearing…",
  "routeList.reset": "Reset",
  "routeList.clear": "Clear",
  "routeList.deleteTitle": "Delete Route",
  "routeList.deleteDescLead": "Are you sure you want to delete route ",
  "routeList.deleteDescTail": "? This action cannot be undone.",
  "routeList.deleting": "Deleting…",
  "routeList.editTitle": "Edit Route",
  "routeList.editDescLead": "Modify routing rule for ",

  // ── Routes: add / edit forms ────────────────────────────────────
  "routes.form.routeAlias": "Route Alias",
  "routes.form.routeAliasHint": "Leave empty to use the first target's real_model",
  "routes.form.strategy": "Strategy",
  "routes.form.strategyStickyFailover": "Sticky-failover",
  "routes.form.strategyFillFirst": "Fill-first",
  "routes.form.strategyRoundRobin": "Round-robin",
  "routes.form.pipeline": "Pipeline",
  "routes.form.pipelineChat": "Chat",
  "routes.form.pipelineImage": "Image",
  "routes.form.pipelineHint":
    "Image routes send to the provider's image section with a per-route path.",
  "routes.form.apiEndpointLabel": "API Endpoint *",
  "routes.form.apiEndpointHintLead": "Appended to the target provider's ",
  "routes.form.apiEndpointHintMid": " section base_url, e.g. ",
  "routes.form.apiEndpointHintOr": " or ",
  "routes.addForm.adding": "Adding...",
  "routes.addForm.createRoute": "Create Route",

  // ── Routes: target editor ───────────────────────────────────────
  "routes.targetEditor.title": "Targets",
  "routes.targetEditor.addTarget": "Add Target",
  "routes.targetEditor.priority": "Priority",
  "routes.targetEditor.retries": "Retries",
  "routes.targetEditor.extra": "Extra (JSON)",
  "routes.targetEditor.selectProvider": "Select provider...",
  "routes.targetEditor.searchProvider": "Search provider...",
  "routes.targetEditor.noProviders": "No providers found",
  "routes.targetEditor.selectModel": "Select model",
  "routes.targetEditor.searchModel": "Search model...",
  "routes.targetEditor.noModels": "No models found",
  "routes.targetEditor.openConfig": "Open configuration",
  "routes.targetEditor.closeConfig": "Close configuration",
  "routes.targetEditor.removeTarget": "Remove target",
  "routes.targetEditor.validJson": "Valid JSON",
  "routes.targetEditor.noApis": "no apis",
  "routes.targetEditor.providerDisabledSuffix": " — Disabled",
  "routes.targetEditor.providerNotFound": "{name} (Not Found)",
  "routes.targetEditor.modelUnavailable": "{model} (Unavailable / Not in provider)",

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

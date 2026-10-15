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
} as const

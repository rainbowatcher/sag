import type { MessageKey } from "@/lib/i18n"

/**
 * Settings tabs, in display order. `value` is the `?tab=` query value used by
 * both the settings page and the sidebar sub-menu; `key` is its i18n label.
 */
export const SETTINGS_TABS = [
  { value: "network", key: "settings.tabs.network" },
  { value: "auth", key: "settings.tabs.authentication" },
  { value: "backoff", key: "settings.tabs.backoff" },
  { value: "breaker", key: "settings.tabs.breaker" },
  { value: "timers", key: "settings.tabs.timers" },
  { value: "analytics", key: "settings.tabs.analytics" },
  { value: "system", key: "settings.tabs.system" },
  { value: "appearance", key: "settings.tabs.appearance" },
] as const satisfies readonly { value: string; key: MessageKey }[]

export type SettingsTab = (typeof SETTINGS_TABS)[number]["value"]

/** Tab shown when `/settings` is opened without a valid `?tab=` value. */
export const DEFAULT_SETTINGS_TAB: SettingsTab = "network"

/** Type guard for a raw `?tab=` query value. */
export function isSettingsTab(value: unknown): value is SettingsTab {
  return typeof value === "string" && SETTINGS_TABS.some((tab) => tab.value === value)
}

/** Resolve a raw `?tab=` query value, falling back to the default tab. */
export function resolveSettingsTab(value: unknown): SettingsTab {
  return isSettingsTab(value) ? value : DEFAULT_SETTINGS_TAB
}

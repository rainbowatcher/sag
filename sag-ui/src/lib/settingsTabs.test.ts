import assert from "node:assert/strict"
import {
  DEFAULT_SETTINGS_TAB,
  isSettingsTab,
  resolveSettingsTab,
  SETTINGS_TABS,
} from "./settingsTabs.ts"

// The `?tab=` query value is the single source of truth shared by SettingsPage
// and the sidebar sub-menu. These tests pin the allowed set (so a rename can't
// silently drop a tab) and the fallback used for a bare `/settings`.

// ── display order ──────────────────────────────────────────────────
{
  assert.deepEqual(
    SETTINGS_TABS.map((tab) => tab.value),
    ["network", "auth", "backoff", "breaker", "timers", "analytics", "system", "appearance"],
    "settings tabs keep their declared order (sidebar sub-menu renders it verbatim)",
  )
  assert.equal(DEFAULT_SETTINGS_TAB, "network")
}

// ── isSettingsTab ──────────────────────────────────────────────────
{
  assert.equal(isSettingsTab("appearance"), true)
  assert.equal(isSettingsTab("auth"), true)
  // The old standalone tab value was retired in favor of `appearance`.
  assert.equal(isSettingsTab("language"), false)
  assert.equal(isSettingsTab("bogus"), false)
  assert.equal(isSettingsTab(undefined), false, "missing ?tab= is not a valid tab")
  assert.equal(isSettingsTab(null), false)
  assert.equal(isSettingsTab(["network"]), false, "repeated query params stay rejected")
}

// ── resolveSettingsTab ─────────────────────────────────────────────
{
  assert.equal(resolveSettingsTab("breaker"), "breaker")
  assert.equal(resolveSettingsTab(undefined), DEFAULT_SETTINGS_TAB, "bare /settings → default tab")
  assert.equal(resolveSettingsTab(""), DEFAULT_SETTINGS_TAB)
  assert.equal(resolveSettingsTab("bogus"), DEFAULT_SETTINGS_TAB)
}

console.log("# settingsTabs.test: all assertions passed")

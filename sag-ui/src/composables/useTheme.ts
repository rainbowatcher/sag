import { useColorMode } from "@vueuse/core"
import { computed } from "vue"

/**
 * Three-state theme controller: `"auto"` (follow the OS), `"light"` or `"dark"`.
 *
 * The storage key is deliberately left at vueuse's default
 * (`vueuse-color-scheme`): the inline script in `index.html` reads that exact
 * key before first paint to apply the `dark` class and avoid a flash of the
 * wrong theme. The previous `useDark()` wrapper used the same key, so existing
 * persisted preferences keep working unchanged.
 *
 * `modes` reproduces `useDark()`'s DOM contract — only the `dark` class is
 * toggled — so switching to the three-state controller does not start stamping
 * a stray `light` class onto `<html>`.
 */
export const colorMode = useColorMode({
  modes: { dark: "dark", light: "" },
})

/** Persisted selection (`"auto" | "light" | "dark"`), bound by the settings select. */
export const themeMode = colorMode.store

/** Resolved theme: `"auto"` collapses to the current OS preference. */
export const isDark = computed(() => colorMode.value === "dark")

/** Flip to the opposite resolved theme (sidebar footer button). */
export function toggleDark(): void {
  colorMode.value = isDark.value ? "light" : "dark"
}

import { ref, watch } from "vue"
import { isDark } from "./useTheme"

/// The shadcn theme defines `--chart-1` … `--chart-N` as CSS variables (in
/// `style.css`, with separate light/`.dark` values). Chart.js cannot consume
/// CSS variables directly, and the values are oklch — which canvas only
/// supports in recent browsers. We resolve each variable to a concrete
/// `rgb(...)` string via a hidden probe element so the resolved color tracks
/// the active theme (light/dark) automatically.
const CHART_VAR_COUNT = 8

function resolveChartColors(): string[] {
  const colors: string[] = []
  for (let i = 1; i <= CHART_VAR_COUNT; i++) {
    const probe = document.createElement("span")
    probe.style.color = `var(--chart-${i})`
    probe.style.display = "none"
    document.documentElement.appendChild(probe)
    // getComputedStyle().color always returns an rgb()/rgba() string,
    // regardless of how the variable was authored (oklch, hex, hsl, …).
    colors.push(getComputedStyle(probe).color)
    probe.remove()
  }
  return colors
}

// Singleton: resolved once lazily and refreshed when dark mode toggles, so
// every consumer shares one source of truth without stacking watchers.
const chartColors = ref<string[]>(resolveChartColors())
let initialized = false

function ensureInit() {
  if (initialized) return
  initialized = true
  chartColors.value = resolveChartColors()
  // useDark applies the `.dark` class in its own watcher; defer the re-read
  // to the next frame so the class is already applied when we probe.
  watch(isDark, () => {
    requestAnimationFrame(() => {
      chartColors.value = resolveChartColors()
    })
  })
}

export function useChartTheme() {
  ensureInit()
  return { chartColors }
}

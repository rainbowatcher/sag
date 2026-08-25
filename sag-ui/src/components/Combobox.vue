<script setup lang="ts">
import { Check, ChevronDown, Loader2, Search } from "@lucide/vue"
import { computed, ref, watch, nextTick, onMounted, onUnmounted } from "vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Command, CommandGroup, CommandItem, CommandList } from "@/components/ui/command"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { cn } from "@/lib/utils"

export interface ComboboxOption {
  value: string
  label?: string
}

const props = withDefaults(
  defineProps<{
    /** v-model selected value(s) — single string or string[] when multiple */
    modelValue?: string | string[]
    /** List of options */
    options: ComboboxOption[]
    /** Placeholder when nothing selected */
    placeholder?: string
    /** Placeholder for the search input */
    searchPlaceholder?: string
    /** Text shown when no options match the search */
    emptyText?: string
    /** Disable the combobox */
    disabled?: boolean
    /** Show loading state */
    loading?: boolean
    /** Enable multi-select mode */
    multiple?: boolean
  }>(),
  {
    placeholder: "Select...",
    searchPlaceholder: "Search...",
    emptyText: "No results found.",
    disabled: false,
    loading: false,
    multiple: false,
  },
)

const emit = defineEmits<{
  "update:modelValue": [value: string | string[]]
}>()

const open = ref(false)
const searchQuery = ref("")
const searchInputRef = ref<HTMLInputElement>()
const badgeContainer = ref<HTMLDivElement>()
const visibleCount = ref(0)

const filteredOptions = computed(() => {
  if (!searchQuery.value.trim()) return props.options
  const q = searchQuery.value.trim().toLowerCase()
  return props.options.filter((opt) => {
    const label = opt.label ?? opt.value
    return label.toLowerCase().includes(q) || opt.value.toLowerCase().includes(q)
  })
})

const selectedValues = computed(() => selectedArray())

const overflowCount = computed(() => Math.max(0, selectedValues.value.length - visibleCount.value))

// ── Selected items helpers ───────────────────────────────────────
function selectedArray(): string[] {
  if (props.multiple) {
    return Array.isArray(props.modelValue) ? props.modelValue : []
  }
  return typeof props.modelValue === "string" && props.modelValue !== "" ? [props.modelValue] : []
}

function isSelected(value: string): boolean {
  return selectedValues.value.includes(value)
}

// ── Trigger display (single-select only; multiple mode renders badges) ──
const triggerLabel = computed(() => {
  if (!props.modelValue) return null
  const opt = props.options.find((o) => o.value === props.modelValue)
  return (
    opt?.label ??
    opt?.value ??
    (typeof props.modelValue === "string" && props.modelValue !== "" ? props.modelValue : null)
  )
})

// ── Selection handler ────────────────────────────────────────────
function handleSelect(currentValue: string) {
  if (props.multiple) {
    const current = selectedValues.value
    if (current.includes(currentValue)) {
      emit(
        "update:modelValue",
        current.filter((v) => v !== currentValue),
      )
    } else {
      emit("update:modelValue", [...current, currentValue])
    }
    // Keep popover open in multiple mode
  } else {
    const newValue = currentValue === props.modelValue ? "" : currentValue
    emit("update:modelValue", newValue)
    open.value = false
  }
}

// ── Remove a selected item (from badge) ─────────────────────────
function removeItem(value: string) {
  if (!props.multiple) return
  const current = selectedValues.value
  emit(
    "update:modelValue",
    current.filter((v) => v !== value),
  )
}

// ── Overflow detection ────────────────────────────────────────
// Cache each badge's natural width keyed by value. Measurement must NOT depend
// on current visibility: v-show hides overflowed badges via `display:none`,
// whose offsetWidth is 0. Feeding those zeros back into the fit calculation
// makes `count` reach `all.length`, skips the shrink branch, and leaves
// `visibleCount` stale — so the visible badges overflow the trigger. The cache
// lets a currently-hidden badge fall back to its last measured width.
const badgeWidths = new Map<string, number>()

function recalcVisible() {
  const el = badgeContainer.value
  const all = selectedValues.value
  if (!el || all.length === 0) {
    visibleCount.value = 0
    return
  }

  // Reveal every badge so this pass can measure real widths before shrinking.
  visibleCount.value = all.length
  nextTick(() => {
    // Not laid out yet (e.g. inside a collapsed/hidden ancestor). Bail and let
    // the ResizeObserver re-run this once the container gets a real width,
    // otherwise we'd wrongly collapse to a single badge.
    if (el.clientWidth === 0) {
      visibleCount.value = 0
      return
    }

    const gap = 4
    // el.children = one badge per selected value (in order) + optional "+N"
    // chip at the end. Read each badge's width, refreshing the cache when it is
    // actually visible, and fall back to the cache when it is not.
    const widths = all.map((value, i) => {
      const node = el.children[i] as HTMLElement | undefined
      const w = node?.offsetWidth ?? 0
      if (w > 0) badgeWidths.set(value, w)
      return badgeWidths.get(value) ?? w
    })

    let acc = 0
    let count = 0
    for (const w of widths) {
      const inc = w + (count > 0 ? gap : 0)
      if (acc + inc > el.clientWidth) break
      acc += inc
      count++
    }
    // If overflow, reserve room for the "+N" chip.
    if (count < all.length) {
      const plusNW = 40
      while (count > 0 && acc + plusNW > el.clientWidth) {
        count--
        acc -= widths[count] + (count > 0 ? gap : 0)
      }
      // Allow 0 so that when even one badge + "+N" overshoots the container,
      // we fall back to showing just the "+N" chip instead of forcing a
      // clipped badge. Without this floor the container always overflows.
      count = Math.max(0, count)
    }
    visibleCount.value = count
  })
}

let observer: ResizeObserver | null = null
let rafId = 0

// The observer watches the same container whose contents the callback mutates
// (toggling badge visibility), which can trip the browser's "ResizeObserver
// loop" warning. Defer the recalc to the next frame so each resize settles
// before we remeasure, breaking the synchronous feedback loop.
function scheduleRecalc() {
  if (rafId) cancelAnimationFrame(rafId)
  rafId = requestAnimationFrame(() => {
    rafId = 0
    recalcVisible()
  })
}

onMounted(() => {
  observer = new ResizeObserver(scheduleRecalc)
})

watch(open, (isOpen) => {
  if (isOpen) {
    nextTick(() => {
      searchInputRef.value?.focus()
    })
  } else {
    searchQuery.value = ""
  }
})

// The badge container sits behind a `v-if`, so it appears/disappears as the
// selection changes. Attach the observer whenever it shows up (covering the
// initial-mount case too) and detach when it goes away, so width changes always
// trigger a recalc — otherwise `visibleCount` goes stale and the badges
// overflow the trigger button.
watch(
  badgeContainer,
  (el, prev) => {
    if (prev) observer?.unobserve(prev)
    if (el) {
      observer?.observe(el)
      scheduleRecalc()
    }
  },
  { immediate: true },
)

watch(selectedValues, () => scheduleRecalc())

onUnmounted(() => {
  observer?.disconnect()
  if (rafId) cancelAnimationFrame(rafId)
})
</script>

<template>
  <Popover v-model:open="open" as="div">
    <PopoverTrigger as-child>
      <Button
        variant="outline"
        role="combobox"
        :aria-expanded="open"
        :disabled="disabled || loading"
        class="bg-background hover:bg-background border-input min-h-[2.25rem] w-full justify-between overflow-hidden px-3 font-normal"
      >
        <!-- Multi-select badge display -->
        <div
          v-if="multiple && selectedValues.length > 0"
          ref="badgeContainer"
          class="flex min-w-0 flex-1 flex-nowrap items-center gap-1 overflow-hidden"
        >
          <Badge
            v-for="(value, idx) in selectedValues"
            :key="value"
            v-show="idx < visibleCount"
            variant="secondary"
            class="max-w-[150px] shrink-0 overflow-hidden text-ellipsis whitespace-nowrap"
          >
            {{ options.find((o) => o.value === value)?.label ?? value }}
            <button
              type="button"
              class="ml-0.5 inline-flex cursor-pointer items-center rounded-full p-0 text-current/60 hover:text-current"
              @click.stop="removeItem(value)"
              :aria-label="'Remove ' + value"
            >
              <span aria-hidden="true">×</span>
            </button>
          </Badge>
          <Badge v-if="overflowCount > 0" variant="secondary" class="shrink-0 text-xs">
            +{{ overflowCount }}
          </Badge>
        </div>
        <!-- Single / placeholder label -->
        <span
          v-else
          :class="
            cn(
              'truncate',
              (multiple ? selectedArray().length === 0 : !modelValue) && 'text-muted-foreground',
            )
          "
        >
          {{ triggerLabel || placeholder }}
        </span>
        <Loader2
          v-if="loading"
          :size="16"
          class="text-muted-foreground/80 shrink-0 animate-spin"
          aria-hidden="true"
        />
        <ChevronDown
          v-else
          :size="16"
          class="text-muted-foreground/80 shrink-0"
          aria-hidden="true"
        />
      </Button>
    </PopoverTrigger>
    <PopoverContent
      class="border-input w-full min-w-[var(--reka-popper-anchor-width)] p-0"
      align="start"
    >
      <Command>
        <div class="flex h-9 items-center gap-2 border-b px-3">
          <Search class="size-4 shrink-0 opacity-50" />
          <input
            ref="searchInputRef"
            v-model="searchQuery"
            :placeholder="searchPlaceholder"
            class="placeholder:text-muted-foreground flex h-10 w-full rounded-md bg-transparent py-3 text-sm outline-hidden disabled:cursor-not-allowed disabled:opacity-50"
          />
        </div>
        <CommandList>
          <div
            v-if="loading || filteredOptions.length === 0"
            class="text-muted-foreground py-6 text-center text-sm"
          >
            {{ loading ? "Loading..." : emptyText }}
          </div>
          <CommandGroup v-else>
            <CommandItem
              v-for="opt in filteredOptions"
              :key="opt.value"
              :value="opt.value"
              @select="handleSelect(opt.value)"
            >
              {{ opt.label ?? opt.value }}
              <Check v-if="isSelected(opt.value)" :size="16" class="ml-auto" />
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
    </PopoverContent>
  </Popover>
</template>

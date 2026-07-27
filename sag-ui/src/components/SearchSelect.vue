<script setup lang="ts">
import { Check, X } from "@lucide/vue"
import type { HTMLAttributes } from "vue"
import { computed, ref, watch, nextTick, onMounted, onUnmounted } from "vue"
import { Badge } from "@/components/ui/badge"
import { Command, CommandGroup, CommandItem, CommandList } from "@/components/ui/command"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { cn } from "@/lib/utils"

defineOptions({
  inheritAttrs: false,
})

export interface SearchSelectOption {
  value: string
  label?: string
  disabled?: boolean
}

const props = withDefaults(
  defineProps<{
    modelValue: string | string[]
    options: SearchSelectOption[]
    multiple?: boolean
    placeholder?: string
    searchPlaceholder?: string
    emptyText?: string
    disabled?: boolean
    hideClearAll?: boolean
    class?: HTMLAttributes["class"]
  }>(),
  {
    placeholder: "Select...",
    searchPlaceholder: "Search...",
    emptyText: "No results found.",
    disabled: false,
    hideClearAll: false,
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

// Bridge internal value to string[] for unified multi-select logic
const internalValue = computed<string[]>(() => {
  if (props.multiple) return props.modelValue as string[]
  const v = props.modelValue as string
  return v ? [v] : []
})

// Single-select: display label for the selected value
const selectedLabel = computed(() => {
  if (props.multiple) return ""
  const val = props.modelValue as string
  if (!val) return ""
  return props.options.find((o) => o.value === val)?.label ?? val
})

const overflowCount = computed(() => Math.max(0, internalValue.value.length - visibleCount.value))

const filteredOptions = computed(() => {
  if (!searchQuery.value) return props.options
  const q = searchQuery.value.toLowerCase()
  return props.options.filter((opt) => (opt.label ?? opt.value).toLowerCase().includes(q))
})

function isSelected(value: string): boolean {
  return internalValue.value.includes(value)
}

function handleSelect(value: string) {
  if (props.multiple) {
    const current = internalValue.value
    if (current.includes(value)) {
      emit(
        "update:modelValue",
        current.filter((v) => v !== value),
      )
    } else {
      emit("update:modelValue", [...current, value])
    }
  } else {
    emit("update:modelValue", value)
    open.value = false
  }
}

function removeItem(value: string) {
  if (!props.multiple) return
  const current = internalValue.value
  emit(
    "update:modelValue",
    current.filter((v) => v !== value),
  )
  nextTick(() => searchInputRef.value?.focus())
}

function clearAll() {
  if (props.multiple) {
    emit("update:modelValue", [])
  } else {
    emit("update:modelValue", "")
  }
  searchQuery.value = ""
  nextTick(() => searchInputRef.value?.focus())
}

// ── Auto-focus the search input when the popover opens ──────────
watch(open, (isOpen) => {
  if (isOpen) {
    nextTick(() => searchInputRef.value?.focus())
  }
})

// ── Overflow detection (multi-select only) ──────────────────────

const badgeWidths = new Map<string, number>()

function recalcVisible() {
  if (!props.multiple) return
  const el = badgeContainer.value
  const all = internalValue.value
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

function scheduleRecalc() {
  if (!props.multiple) return
  if (rafId) cancelAnimationFrame(rafId)
  rafId = requestAnimationFrame(() => {
    rafId = 0
    recalcVisible()
  })
}

onMounted(() => {
  if (props.multiple) {
    observer = new ResizeObserver(scheduleRecalc)
  }
})

watch(
  badgeContainer,
  (el, prev) => {
    if (!props.multiple) return
    if (prev) observer?.unobserve(prev)
    if (el) {
      observer?.observe(el)
      scheduleRecalc()
    }
  },
  { immediate: true },
)

watch(internalValue, () => {
  if (props.multiple) scheduleRecalc()
})

onUnmounted(() => {
  observer?.disconnect()
  if (rafId) cancelAnimationFrame(rafId)
})
</script>

<template>
  <Popover v-model:open="open">
    <PopoverTrigger as-child>
      <div
        role="combobox"
        :aria-expanded="open"
        :aria-disabled="disabled"
        :class="
          cn(
            'border-input bg-background flex min-h-[2.25rem] w-full cursor-text flex-nowrap items-center gap-1 rounded-md border px-3 py-1 outline-offset-0 outline-none focus-within:outline-[3px]',
            props.class,
          )
        "
      >
        <!-- Single select: search input (opens popover on focus) + selected label -->
        <template v-if="!multiple">
          <span v-if="!open && selectedLabel" class="flex-1 truncate text-sm">
            {{ selectedLabel }}
          </span>
          <input
            v-else
            ref="searchInputRef"
            v-model="searchQuery"
            :placeholder="selectedLabel || placeholder"
            :disabled="disabled"
            class="placeholder:text-muted-foreground min-w-[60px] flex-1 bg-transparent text-sm outline-none"
            :class="{ 'text-muted-foreground': !selectedLabel && !searchQuery }"
            @focus="open = true"
            @click.stop
          />
        </template>

        <!-- Multi select: badge area + search input -->
        <template v-else>
          <div
            v-if="internalValue.length > 0"
            ref="badgeContainer"
            class="flex min-w-0 flex-nowrap items-center gap-1 overflow-hidden"
          >
            <Badge
              v-for="(value, idx) in internalValue"
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
                <X :size="12" />
              </button>
            </Badge>
            <Badge v-if="overflowCount > 0" variant="secondary" class="shrink-0 text-xs">
              +{{ overflowCount }}
            </Badge>
          </div>

          <!-- Search input (multi-mode, in trigger area) -->
          <input
            ref="searchInputRef"
            v-model="searchQuery"
            :placeholder="internalValue.length === 0 ? placeholder : ''"
            :disabled="disabled"
            class="placeholder:text-muted-foreground min-w-[60px] flex-1 bg-transparent text-sm outline-none"
            @focus="open = true"
            @click.stop
          />
        </template>

        <!-- Clear button -->
        <button
          v-if="internalValue.length > 0 && !hideClearAll"
          type="button"
          class="text-muted-foreground/60 hover:text-muted-foreground shrink-0 cursor-pointer"
          @click.stop="clearAll"
          :aria-label="'Clear'"
        >
          <X :size="14" />
        </button>
      </div>
    </PopoverTrigger>
    <PopoverContent
      class="border-input w-full min-w-[var(--reka-popper-anchor-width)] p-0"
      align="start"
    >
      <Command>
        <CommandList>
          <div
            v-if="filteredOptions.length === 0"
            class="text-muted-foreground py-6 text-center text-sm"
          >
            {{ emptyText }}
          </div>
          <CommandGroup v-else>
            <CommandItem
              v-for="opt in filteredOptions"
              :key="opt.value"
              :value="opt.value"
              :disabled="opt.disabled"
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

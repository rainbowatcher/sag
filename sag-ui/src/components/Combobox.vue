<script setup lang="ts">
import { Check, ChevronDown, Loader2 } from "@lucide/vue"
import { computed, ref } from "vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command"
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

// ── Selected items helpers ───────────────────────────────────────
function selectedArray(): string[] {
  if (props.multiple) {
    return Array.isArray(props.modelValue) ? props.modelValue : []
  }
  return typeof props.modelValue === "string" && props.modelValue !== "" ? [props.modelValue] : []
}

function isSelected(value: string): boolean {
  return selectedArray().includes(value)
}

// ── Trigger display ──────────────────────────────────────────────
const triggerLabel = computed(() => {
  if (props.multiple) {
    if (selectedArray().length === 0) return null
    return `${selectedArray().length} selected`
  }
  // Single mode
  if (!props.modelValue) return null
  const opt = props.options.find((o) => o.value === props.modelValue)
  const label = opt?.label ?? opt?.value
  return label ?? null
})

// ── Selection handler ────────────────────────────────────────────
function handleSelect(currentValue: string) {
  if (props.multiple) {
    const current = selectedArray()
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
  const current = selectedArray()
  emit(
    "update:modelValue",
    current.filter((v) => v !== value),
  )
}
</script>

<template>
  <Popover v-model:open="open" as="div">
    <PopoverTrigger as-child>
      <Button
        variant="outline"
        role="combobox"
        :aria-expanded="open"
        :disabled="disabled || loading"
        class="bg-background hover:bg-background border-input min-h-[2.25rem] w-full justify-between px-3 font-normal outline-offset-0 outline-none focus-visible:outline-[3px]"
      >
        <!-- Multi-select badge display -->
        <div v-if="multiple && selectedArray().length > 0" class="flex flex-1 flex-wrap gap-1">
          <Badge
            v-for="value in selectedArray()"
            :key="value"
            variant="secondary"
            class="max-w-[150px] overflow-hidden text-ellipsis whitespace-nowrap"
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
        <CommandInput :placeholder="searchPlaceholder" />
        <CommandList>
          <CommandEmpty>{{ loading ? "Loading..." : emptyText }}</CommandEmpty>
          <CommandGroup>
            <CommandItem
              v-for="opt in options"
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

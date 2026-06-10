<script setup lang="ts">
import { Check, ChevronDown, Loader2 } from "lucide-vue-next"
import { computed, ref } from "vue"
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
    /** v-model selected value */
    modelValue?: string
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
  }>(),
  {
    placeholder: "Select...",
    searchPlaceholder: "Search...",
    emptyText: "No results found.",
    disabled: false,
    loading: false,
  },
)

const emit = defineEmits<{
  "update:modelValue": [value: string]
}>()

const open = ref(false)

const selectedLabel = computed(() => {
  const opt = props.options.find((o) => o.value === props.modelValue)
  return opt?.label ?? opt?.value
})

function handleSelect(currentValue: string) {
  const newValue = currentValue === props.modelValue ? "" : currentValue
  emit("update:modelValue", newValue)
  open.value = false
}
</script>

<template>
  <Popover v-model:open="open">
    <PopoverTrigger as-child>
      <Button
        variant="outline"
        role="combobox"
        :aria-expanded="open"
        :disabled="disabled || loading"
        class="bg-background hover:bg-background border-input w-full justify-between px-3 font-normal outline-offset-0 outline-none focus-visible:outline-[3px]"
      >
        <span :class="cn('truncate', !modelValue && 'text-muted-foreground')">
          {{ selectedLabel || placeholder }}
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
              <Check v-if="modelValue === opt.value" :size="16" class="ml-auto" />
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
    </PopoverContent>
  </Popover>
</template>

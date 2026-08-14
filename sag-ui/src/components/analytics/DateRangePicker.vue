<script setup lang="ts">
import { CalendarDate } from "@internationalized/date"
import { Calendar } from "@lucide/vue"
import type { DateRange } from "reka-ui"
import type { HTMLAttributes } from "vue"
import { computed, ref } from "vue"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { RangeCalendar } from "@/components/ui/range-calendar"
import { cn } from "@/lib/utils"

export interface DateRangeMs {
  /** UTC ms of the start day's 00:00. */
  fromMs: number
  /** UTC ms of the end day's 23:59:59.999. */
  toMs: number
}

const props = withDefaults(
  defineProps<{
    modelValue: DateRangeMs | null
    placeholder?: string
    class?: HTMLAttributes["class"]
  }>(),
  { placeholder: "Pick a range" },
)

const emit = defineEmits<{
  "update:modelValue": [value: DateRangeMs | null]
}>()

const open = ref(false)

/**
 * Backend buckets are UTC hours, so the picked calendar dates are
 * interpreted as UTC boundaries: from = start day 00:00 UTC, to = end day
 * 23:59:59.999 UTC. This keeps the custom range aligned with the hourly
 * trend buckets. A partial pick (only one bound) yields `null` — the
 * consumer disables the query until both bounds are chosen.
 */
function toDateRangeMs(range: DateRange | null): DateRangeMs | null {
  if (!range?.start || !range.end) return null
  const { start, end } = range
  return {
    fromMs: Date.UTC(start.year, start.month - 1, start.day),
    toMs: Date.UTC(end.year, end.month - 1, end.day, 23, 59, 59, 999),
  }
}

/** Reverse mapping so the calendar re-opens showing the stored range. */
const calendarValue = computed<DateRange | null>(() => {
  const v = props.modelValue
  if (!v) return null
  const f = new Date(v.fromMs)
  const t = new Date(v.toMs)
  return {
    start: new CalendarDate(f.getUTCFullYear(), f.getUTCMonth() + 1, f.getUTCDate()),
    end: new CalendarDate(t.getUTCFullYear(), t.getUTCMonth() + 1, t.getUTCDate()),
  }
})

/** reka-ui's model-value payload type is wide; accept unknown and cast. */
function onCalendarUpdate(value: unknown) {
  const range = toDateRangeMs(value as DateRange | null)
  emit("update:modelValue", range)
  // A range is only emitted once both bounds are picked; close then.
  if (range) open.value = false
}

const displayLabel = computed(() => {
  const v = props.modelValue
  if (!v) return ""
  const fmt = (d: Date) =>
    d.toLocaleDateString("en-CA", { year: "numeric", month: "short", day: "numeric" })
  return `${fmt(new Date(v.fromMs))} – ${fmt(new Date(v.toMs))}`
})
</script>

<template>
  <Popover v-model:open="open">
    <PopoverTrigger as-child>
      <div
        role="combobox"
        :aria-expanded="open"
        :class="
          cn(
            'border-input bg-background flex min-h-[2.25rem] w-56 shrink-0 cursor-pointer items-center gap-1.5 rounded-md border px-3 py-1 whitespace-nowrap',
            !modelValue && 'text-muted-foreground',
            props.class,
          )
        "
      >
        <Calendar class="h-4 w-4 shrink-0" />
        <span class="min-w-0 truncate">{{ displayLabel || placeholder }}</span>
      </div>
    </PopoverTrigger>
    <PopoverContent class="w-auto p-0" align="start">
      <RangeCalendar
        :model-value="calendarValue"
        :number-of-months="2"
        @update:model-value="onCalendarUpdate"
      />
    </PopoverContent>
  </Popover>
</template>

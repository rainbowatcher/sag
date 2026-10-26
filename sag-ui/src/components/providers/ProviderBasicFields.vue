<script setup lang="ts">
import { Info } from "@lucide/vue"
import { computed } from "vue"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import { FieldLabel } from "@/components/ui/field"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import type { ProxyMode } from "@/types/api"

defineProps<{
  apiKeys: string[]
  keyStrategy: "fill_first" | "round_robin"
  proxyMode: ProxyMode
}>()

const emit = defineEmits<{
  "update:apiKeys": [value: string[]]
  "update:keyStrategy": [value: "fill_first" | "round_robin"]
  "update:proxyMode": [value: ProxyMode]
}>()

// reka-ui captures each `SelectItem`'s textContent once at mount and `SelectValue`
// renders that frozen string, so its default output never re-translates on a
// locale switch. These locale-reactive maps feed `SelectValue` an explicit slot
// label (same `t()` keys as the items) instead of the captured text.
const keyStrategyLabels = computed<Record<"fill_first" | "round_robin", string>>(() => ({
  fill_first: t("providers.form.fillFirst"),
  round_robin: t("providers.form.roundRobin"),
}))

const proxyModeLabels = computed<Record<ProxyMode, string>>(() => ({
  inherit: t("common.inherit"),
  enabled: t("common.enabled"),
  disabled: t("common.disabled"),
}))
</script>

<template>
  <div class="col-span-2">
    <ApiKeyEditor :api-keys="apiKeys" @update:api-keys="emit('update:apiKeys', $event)" />
  </div>

  <div class="flex items-center justify-between gap-4">
    <FieldLabel class="shrink-0">{{ $t("providers.form.keyStrategy") }}</FieldLabel>
    <div class="w-44">
      <Select
        :model-value="keyStrategy"
        @update:model-value="(v) => emit('update:keyStrategy', v as 'fill_first' | 'round_robin')"
      >
        <SelectTrigger class="w-full">
          <SelectValue>{{ keyStrategyLabels[keyStrategy] }}</SelectValue>
        </SelectTrigger>
        <SelectContent class="w-(--reka-select-trigger-width)">
          <SelectItem value="fill_first">{{ $t("providers.form.fillFirst") }}</SelectItem>
          <SelectItem value="round_robin">{{ $t("providers.form.roundRobin") }}</SelectItem>
        </SelectContent>
      </Select>
    </div>
  </div>

  <div class="flex items-center justify-between gap-4">
    <FieldLabel class="flex shrink-0 items-center gap-1.5">
      {{ $t("providers.form.proxyMode") }}
      <Tooltip>
        <TooltipTrigger as-child>
          <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
        </TooltipTrigger>
        <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
          {{ $t("providers.form.proxyModeHelp") }}
        </TooltipContent>
      </Tooltip>
    </FieldLabel>
    <div class="w-44">
      <Select
        :model-value="proxyMode"
        @update:model-value="(v) => emit('update:proxyMode', v as ProxyMode)"
      >
        <SelectTrigger class="w-full">
          <SelectValue>{{ proxyModeLabels[proxyMode] }}</SelectValue>
        </SelectTrigger>
        <SelectContent class="w-(--reka-select-trigger-width)">
          <SelectItem value="inherit">{{ $t("common.inherit") }}</SelectItem>
          <SelectItem value="enabled">{{ $t("common.enabled") }}</SelectItem>
          <SelectItem value="disabled">{{ $t("common.disabled") }}</SelectItem>
        </SelectContent>
      </Select>
    </div>
  </div>
</template>

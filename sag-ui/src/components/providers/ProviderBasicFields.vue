<script setup lang="ts">
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import { FieldLabel } from "@/components/ui/field"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
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
</script>

<template>
  <div class="col-span-2">
    <ApiKeyEditor :api-keys="apiKeys" @update:api-keys="emit('update:apiKeys', $event)" />
  </div>

  <div class="space-y-1">
    <FieldLabel>Key Strategy</FieldLabel>
    <Select
      :model-value="keyStrategy"
      @update:model-value="(v) => emit('update:keyStrategy', v as 'fill_first' | 'round_robin')"
    >
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem value="fill_first">Fill First</SelectItem>
        <SelectItem value="round_robin">Round Robin</SelectItem>
      </SelectContent>
    </Select>
  </div>

  <div class="space-y-1">
    <FieldLabel>Proxy Mode</FieldLabel>
    <Select
      :model-value="proxyMode"
      @update:model-value="(v) => emit('update:proxyMode', v as ProxyMode)"
    >
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem value="inherit">Inherit</SelectItem>
        <SelectItem value="enabled">Enabled</SelectItem>
        <SelectItem value="disabled">Disabled</SelectItem>
      </SelectContent>
    </Select>
    <p class="text-muted-foreground text-xs">
      How this provider resolves the upstream proxy. Inherit follows the gateway-level proxy URL.
    </p>
  </div>
</template>

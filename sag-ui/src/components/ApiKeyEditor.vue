<script setup lang="ts">
import { Plus, X } from "@lucide/vue"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"

const props = withDefaults(
  defineProps<{
    apiKeys: string[]
    keyStrategy: "fill_first" | "round_robin"
    disabled?: boolean
  }>(),
  {
    disabled: false,
  },
)

const emit = defineEmits<{
  "update:apiKeys": [value: string[]]
  "update:keyStrategy": [value: "fill_first" | "round_robin"]
}>()

function addApiKey() {
  emit("update:apiKeys", [...props.apiKeys, ""])
}

function removeApiKey(idx: number) {
  const next = props.apiKeys.filter((_, i) => i !== idx)
  if (next.length === 0) next.push("")
  emit("update:apiKeys", next)
}

function updateApiKey(idx: number, value: string) {
  const next = props.apiKeys.map((k, i) => (i === idx ? value : k))
  emit("update:apiKeys", next)
}
</script>

<template>
  <div class="space-y-2">
    <FieldLabel>API Keys</FieldLabel>
    <div v-for="(_, idx) in apiKeys" :key="idx" class="flex items-center gap-2">
      <Input
        type="password"
        :model-value="apiKeys[idx]"
        :placeholder="`sk-... (key ${idx + 1})`"
        class="flex-1"
        :disabled="disabled"
        @input="updateApiKey(idx, ($event.target as HTMLInputElement).value)"
      />
      <Button variant="ghost" size="icon-sm" :disabled="disabled" @click="removeApiKey(idx)">
        <X class="h-3.5 w-3.5" />
      </Button>
    </div>
    <Button type="button" variant="outline" size="sm" :disabled="disabled" @click="addApiKey">
      <Plus class="h-3.5 w-3.5" />
      Add Key
    </Button>

    <FieldLabel>Key Strategy</FieldLabel>
    <Select
      :model-value="keyStrategy"
      :disabled="disabled"
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
</template>

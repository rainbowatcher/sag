<script setup lang="ts">
import { Plus, X } from "@lucide/vue"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

defineOptions({
  inheritAttrs: false,
})

interface KvPair {
  key: string
  value: string
}

const props = withDefaults(
  defineProps<{
    title?: string
    description?: string
    modelValue: KvPair[]
    keyPlaceholder?: string
    valuePlaceholder?: string
    disabled?: boolean
  }>(),
  {
    title: "",
    description: "",
    keyPlaceholder: "Key",
    valuePlaceholder: "Value",
    disabled: false,
  },
)

const emit = defineEmits<{
  "update:modelValue": [value: KvPair[]]
}>()

function addKvPair() {
  emit("update:modelValue", [...props.modelValue, { key: "", value: "" }])
}

function removeKvPair(idx: number) {
  const next = props.modelValue.filter((_, i) => i !== idx)
  emit("update:modelValue", next)
}

function updateKey(idx: number, value: string) {
  const next = props.modelValue.map((pair, i) => (i === idx ? { ...pair, key: value } : pair))
  emit("update:modelValue", next)
}

function updateValue(idx: number, value: string) {
  const next = props.modelValue.map((pair, i) => {
    if (i !== idx) return pair
    const updated: KvPair = Object.assign({}, pair)
    updated.value = value
    return updated
  })
  emit("update:modelValue", next)
}
</script>

<template>
  <div class="space-y-1">
    <p
      v-if="title"
      class="text-sm leading-none font-medium peer-disabled:cursor-not-allowed peer-disabled:opacity-70"
    >
      {{ title }}
    </p>
    <p v-if="description" class="text-muted-foreground text-xs">
      {{ description }}
    </p>
    <div v-for="(pair, idx) in modelValue" :key="idx" class="mb-1 flex items-center gap-2">
      <Input
        :model-value="pair.key"
        :placeholder="keyPlaceholder"
        class="w-48"
        :disabled="disabled"
        @input="updateKey(idx, ($event.target as HTMLInputElement).value)"
      />
      <Input
        :model-value="pair.value"
        :placeholder="valuePlaceholder"
        class="flex-1"
        :disabled="disabled"
        @input="updateValue(idx, ($event.target as HTMLInputElement).value)"
      />
      <Button variant="ghost" size="icon-sm" :disabled="disabled" @click="removeKvPair(idx)">
        <X class="h-3.5 w-3.5" />
      </Button>
    </div>
    <Button type="button" variant="outline" size="sm" :disabled="disabled" @click="addKvPair">
      <Plus class="h-3.5 w-3.5" />
      Add
    </Button>
  </div>
</template>

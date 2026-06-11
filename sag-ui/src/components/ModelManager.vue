<script setup lang="ts">
import { Loader2, Plus, RefreshCw, X } from "lucide-vue-next"
import { ref, type Ref } from "vue"
import Combobox from "@/components/Combobox.vue"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"

function modelLabel(item: Record<string, unknown>): string {
  return (item.id ?? item.name ?? item.model ?? Object.values(item)[0]) as string
}

export interface ModelTestState {
  loading: boolean
  error: string
  data?: { key_prefix: string; success: boolean }[]
}

const props = withDefaults(
  defineProps<{
    enabledModels: string[]
    upstreamModels: Record<string, unknown>[]
    upstreamModelsLoading: boolean
    upstreamModelsError: string
    /** Per-model test states (optional — only shown when provided) */
    modelTestStates?: Record<string, ModelTestState | undefined>
    disabled?: boolean
  }>(),
  {
    modelTestStates: undefined,
    disabled: false,
  },
)

const emit = defineEmits<{
  "update:enabledModels": [value: string[]]
  fetchModels: [modelsEndpoint: string]
  testModel: [model: string]
}>()

// ── Internal state ─────────────────────────────────
const selectedModel = ref("")
const customModel = ref("")
const modelsEndpoint = ref("models")

function addModelByName(name: string, sourceRef: Ref<string>) {
  const trimmed = name.trim()
  if (trimmed && !props.enabledModels.includes(trimmed)) {
    emit("update:enabledModels", [...props.enabledModels, trimmed])
  }
  sourceRef.value = ""
}

function addModel() {
  addModelByName(selectedModel.value, selectedModel)
}

function addCustomModel() {
  addModelByName(customModel.value, customModel)
}

function removeModel(name: string) {
  emit(
    "update:enabledModels",
    props.enabledModels.filter((m) => m !== name),
  )
}
</script>

<template>
  <div class="space-y-3">
    <p v-if="enabledModels.length === 0" class="text-muted-foreground text-xs">
      No models listed means all models are allowed (no restriction).
    </p>

    <!-- Model list -->
    <div v-if="enabledModels.length > 0" class="space-y-1">
      <div
        v-for="model in enabledModels"
        :key="model"
        class="bg-muted flex flex-wrap items-center gap-2 rounded px-2 py-1 text-xs"
      >
        <span class="font-mono">{{ model }}</span>

        <!-- Test button (only if modelTestStates provided) -->
        <Button
          v-if="modelTestStates"
          variant="ghost"
          size="icon-sm"
          :disabled="modelTestStates[model]?.loading"
          @click="emit('testModel', model)"
        >
          <Loader2 v-if="modelTestStates[model]?.loading" class="h-3 w-3 animate-spin" />
          <RefreshCw v-else class="h-3 w-3" />
        </Button>

        <!-- Test results -->
        <template v-if="modelTestStates?.[model]">
          <div v-if="modelTestStates[model]!.data" class="flex flex-wrap items-center gap-2">
            <span
              v-for="r in modelTestStates[model]!.data"
              :key="r.key_prefix"
              class="rounded px-1 py-0.5"
              :class="r.success ? 'bg-green-100 text-green-700' : 'bg-red-100 text-red-700'"
            >
              {{ r.success ? "✓" : "✗" }} {{ r.key_prefix }}
            </span>
          </div>
          <div v-if="modelTestStates[model]!.error" class="w-full text-xs text-red-500">
            {{ modelTestStates[model]!.error }}
          </div>
        </template>

        <Button variant="ghost" size="icon-sm" @click="removeModel(model)" class="ml-auto">
          <X class="h-3 w-3" />
        </Button>
      </div>
    </div>

    <!-- Custom model input -->
    <div class="flex items-center gap-2">
      <Input
        :model-value="customModel"
        placeholder="Or type a model name..."
        class="flex-1"
        :disabled="disabled"
        @input="customModel = ($event.target as HTMLInputElement).value"
      />
      <Button
        type="button"
        variant="outline"
        size="sm"
        :disabled="!customModel.trim()"
        @click="addCustomModel"
      >
        <Plus class="h-3.5 w-3.5" />
        Add
      </Button>
    </div>

    <!-- Combobox + Add -->
    <div class="flex gap-2">
      <div class="min-w-0 flex-1">
        <Combobox
          v-model="selectedModel"
          :options="
            upstreamModels
              .filter((m) => !enabledModels.includes(modelLabel(m)))
              .map((m) => ({ value: modelLabel(m) }))
          "
          placeholder="Select a model..."
          search-placeholder="Search models..."
          empty-text="No models found. Click Refresh Models."
          :loading="upstreamModelsLoading"
        />
      </div>
      <Button variant="outline" size="sm" :disabled="!selectedModel.trim()" @click="addModel">
        <Plus class="h-3.5 w-3.5" />
      </Button>
    </div>

    <!-- Refresh + loading / error -->
    <div class="flex items-center gap-2">
      <Button
        type="button"
        variant="outline"
        size="sm"
        :disabled="disabled"
        @click="emit('fetchModels', modelsEndpoint)"
      >
        <RefreshCw class="h-3.5 w-3.5" />
        Refresh Models
      </Button>
      <div v-if="upstreamModelsLoading" class="text-muted-foreground text-xs">
        Loading models...
      </div>
    </div>

    <div v-if="upstreamModelsError" class="rounded-md bg-red-50 px-2 py-1 text-xs text-red-500">
      {{ upstreamModelsError }}
    </div>

    <!-- Models endpoint path -->
    <div class="space-y-1">
      <FieldLabel>Models Endpoint Path</FieldLabel>
      <p class="text-muted-foreground text-xs">
        Path appended to base_url for fetching models (default: models).
      </p>
      <Input
        :model-value="modelsEndpoint"
        :disabled="disabled"
        @input="modelsEndpoint = ($event.target as HTMLInputElement).value"
      />
    </div>
  </div>
</template>

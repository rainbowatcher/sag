<script setup lang="ts">
import { Loader2, RefreshCw, Trash2, Zap } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import Combobox from "@/components/Combobox.vue"
import KvEditor from "@/components/KvEditor.vue"
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from "@/components/ui/accordion"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"

interface KvPair {
  key: string
  value: string
}

function modelLabel(item: Record<string, unknown>): string {
  return (item.id ?? item.name ?? item.model ?? Object.values(item)[0]) as string
}

export interface ModelTestState {
  loading: boolean
  error: string
  data?: { key_prefix: string; success: boolean; message: string }[]
}

const props = withDefaults(
  defineProps<{
    enabledModels: string[]
    modelOverrides: Record<string, KvPair[]>
    upstreamModels: Record<string, unknown>[]
    upstreamModelsLoading: boolean
    upstreamModelsError: string
    modelsEndpoint?: string
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
  "update:modelOverrides": [value: Record<string, KvPair[]>]
  "update:modelsEndpoint": [value: string]
  fetchModels: [modelsEndpoint: string]
  testModel: [model: string]
}>()

// ── Internal state ─────────────────────────────────
const modelsEndpoint = computed(() => props.modelsEndpoint ?? "models")
const expandedOverride = ref<string | undefined>(undefined)

// ── Clean up overrides when model removed ───────────
watch(
  () => props.enabledModels,
  (models) => {
    const updated = { ...props.modelOverrides }
    let changed = false
    for (const model of Object.keys(updated)) {
      if (!models.includes(model)) {
        delete updated[model]
        changed = true
        if (expandedOverride.value === model) expandedOverride.value = undefined
      }
    }
    if (changed) emit("update:modelOverrides", updated)
  },
)

function overrideCount(model: string): number {
  return props.modelOverrides[model]?.filter((p) => p.key.trim()).length ?? 0
}

function onOverrideUpdate(model: string, pairs: KvPair[]) {
  emit("update:modelOverrides", { ...props.modelOverrides, [model]: pairs })
}

function removeModel(name: string) {
  emit(
    "update:enabledModels",
    props.enabledModels.filter((m) => m !== name),
  )
}

function onModelsEndpointChange(value: string | number) {
  emit("update:modelsEndpoint", String(value))
}
</script>

<template>
  <div class="space-y-3">
    <p v-if="enabledModels.length === 0" class="text-muted-foreground text-xs">
      No models listed means all models are allowed (no restriction).
    </p>

    <!-- Model list with integrated override editing -->
    <Accordion
      v-if="enabledModels.length > 0"
      type="single"
      collapsible
      v-model="expandedOverride"
      class="rounded-md border"
    >
      <AccordionItem v-for="model in enabledModels" :key="model" :value="model">
        <div class="hover:bg-muted/50 flex items-center [&>:first-child]:flex-1">
          <AccordionTrigger class="px-3 py-0.5 hover:no-underline">
            <span class="flex items-center gap-2">
              <span class="font-mono text-sm">{{ model }}</span>
              <Badge
                v-if="overrideCount(model) > 0"
                variant="secondary"
                class="px-1 py-0 text-[10px] font-medium"
              >
                {{ overrideCount(model) }}
              </Badge>
            </span>
          </AccordionTrigger>
          <div class="flex items-center gap-1 pr-3">
            <Tooltip v-if="modelTestStates">
              <TooltipTrigger as-child>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  :disabled="modelTestStates[model]?.loading"
                  :class="
                    modelTestStates[model]?.data?.length
                      ? modelTestStates[model]!.data![0].success
                        ? 'text-green-500'
                        : 'text-destructive'
                      : ''
                  "
                  @click="emit('testModel', model)"
                >
                  <Loader2 v-if="modelTestStates[model]?.loading" class="h-3 w-3 animate-spin" />
                  <Zap v-else class="h-3 w-3" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="bottom" align="end" class="max-w-xs">
                <template v-if="modelTestStates[model]?.data">
                  <div
                    v-for="r in modelTestStates[model]!.data"
                    :key="r.key_prefix"
                    class="break-words whitespace-normal"
                  >
                    <span :class="r.success ? 'text-green-300' : 'text-red-300'">{{
                      r.success ? "✓" : "✗"
                    }}</span>
                    {{ r.key_prefix }}: {{ r.message }}
                  </div>
                </template>
                <template v-else-if="modelTestStates[model]?.error">
                  <div class="break-words whitespace-normal text-red-300">
                    {{ modelTestStates[model]!.error }}
                  </div>
                </template>
                <template v-else> Click to test </template>
              </TooltipContent>
            </Tooltip>

            <Button variant="ghost" size="icon-sm" @click="removeModel(model)">
              <Trash2 class="text-destructive h-3 w-3" />
            </Button>
          </div>
        </div>
        <AccordionContent class="border-t px-3 py-3">
          <KvEditor
            :model-value="modelOverrides[model] ?? []"
            @update:model-value="(v) => onOverrideUpdate(model, v as KvPair[])"
            description='Values are parsed as JSON (e.g. 4096, true, "text"). Applied after provider-level overrides.'
            key-placeholder="max_tokens"
            value-placeholder="4096"
          />
        </AccordionContent>
      </AccordionItem>
    </Accordion>

    <!-- Multi-select model combobox -->
    <div class="min-w-0">
      <Combobox
        :model-value="enabledModels"
        :options="upstreamModels.map((m) => ({ value: modelLabel(m) }))"
        placeholder="Select models..."
        search-placeholder="Search models..."
        empty-text="No models found. Click Refresh Models."
        :loading="upstreamModelsLoading"
        multiple
        @update:model-value="(v) => emit('update:enabledModels', v as string[])"
      />
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
        @update:model-value="onModelsEndpointChange"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { GitBranch, Loader2, Plus, RefreshCw, Trash2, Zap } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import { toast } from "vue-sonner"
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
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { getErrorMessage } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { CreateRoutePayload } from "@/types/api"

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
    providerName?: string
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
    providerName: "",
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

const routeStore = useRouteStore()

// ── Quick Create Route Dialog ──────────────────────
const routeToCreate = ref<string | null>(null)
const customRouteName = ref("")
const creatingRoute = ref(false)

const isRouteNameEmpty = computed(() => !customRouteName.value.trim())

const isRouteExisting = computed(() => {
  const name = customRouteName.value.trim()
  if (!name) return false
  return routeStore.routes.some((r) => r.model_name === name)
})

const canConfirmRoute = computed(() => {
  return !isRouteNameEmpty.value && !isRouteExisting.value && !creatingRoute.value
})

function openCreateRoute(model: string) {
  routeToCreate.value = model
  customRouteName.value = props.providerName ? `${props.providerName}/${model}` : model
  if (routeStore.routes.length === 0) {
    routeStore.fetchRoutes()
  }
}

async function confirmCreateRoute() {
  if (!props.providerName || !routeToCreate.value) return
  const routeName = customRouteName.value.trim()
  if (!routeName || isRouteExisting.value) return

  creatingRoute.value = true
  try {
    const payload: CreateRoutePayload = {
      model_name: routeName,
      api_endpoint: "/chat/completions",
      strategy: "sticky_failover",
      targets: [
        {
          provider: props.providerName,
          real_model: routeToCreate.value,
          priority: 0,
        },
      ],
    }
    await routeStore.createRoute(payload)
    toast.success(`Route "${routeName}" created`)
    routeToCreate.value = null
    await routeStore.fetchRoutes()
  } catch (e: any) {
    toast.error(getErrorMessage(e, "Failed to create route"))
  } finally {
    creatingRoute.value = false
  }
}

// ── Internal state ─────────────────────────────────
const modelsEndpoint = computed(() => props.modelsEndpoint ?? "models")
const expandedOverride = ref<string | undefined>(undefined)
const customModel = ref("")

const upstreamModelNames = computed(() => new Set(props.upstreamModels.map((m) => modelLabel(m))))
const hasFetchedUpstream = computed(() => props.upstreamModels.length > 0)

function isModelMissingFromUpstream(model: string): boolean {
  return hasFetchedUpstream.value && !upstreamModelNames.value.has(model)
}

const hasFetched = ref(false)

watch(modelsEndpoint, () => {
  hasFetched.value = false
})

watch(
  () => props.upstreamModels,
  (models) => {
    if (models.length > 0) hasFetched.value = true
  },
)

const comboboxOptions = computed(() => {
  const upstreamList = props.upstreamModels.map((m) => {
    const val = modelLabel(m)
    return { value: val, label: val }
  })
  const existingSet = new Set(upstreamList.map((o) => o.value))

  const missingList: { value: string; label: string; warning?: boolean }[] = []
  if (hasFetchedUpstream.value) {
    for (const m of props.enabledModels) {
      if (!existingSet.has(m)) {
        missingList.push({
          value: m,
          label: m,
          warning: true,
        })
      }
    }
  }

  return [...missingList, ...upstreamList]
})

function onComboboxOpen() {
  if (!hasFetched.value && !props.upstreamModelsLoading && props.upstreamModels.length === 0) {
    hasFetched.value = true
    emit("fetchModels", modelsEndpoint.value)
  }
}

function handleManualRefresh() {
  hasFetched.value = true
  emit("fetchModels", modelsEndpoint.value)
}

function addCustomModel() {
  const trimmed = customModel.value.trim()
  if (trimmed && !props.enabledModels.includes(trimmed)) {
    emit("update:enabledModels", [...props.enabledModels, trimmed])
  }
  customModel.value = ""
}

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
      <AccordionItem
        v-for="model in enabledModels"
        :key="model"
        :value="model"
        :class="{
          'text-muted-foreground bg-amber-500/10 dark:bg-amber-500/15':
            isModelMissingFromUpstream(model),
        }"
      >
        <div class="hover:bg-muted/50 flex items-center [&>:first-child]:flex-1">
          <AccordionTrigger class="px-3 py-0.5 hover:no-underline">
            <template #icon><span class="hidden" /></template>
            <span class="flex items-center gap-2">
              <span
                class="font-mono text-sm"
                :class="{
                  'font-medium text-amber-700 dark:text-amber-300':
                    isModelMissingFromUpstream(model),
                }"
              >
                {{ model }}
              </span>
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

            <Tooltip v-if="providerName">
              <TooltipTrigger as-child>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  title="Create route"
                  aria-label="Create route"
                  @click.stop="openCreateRoute(model)"
                >
                  <GitBranch class="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent side="bottom" align="end" class="text-xs">
                Create route {{ providerName }}/{{ model }}
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
        :options="comboboxOptions"
        placeholder="Select models..."
        search-placeholder="Search models..."
        empty-text="No models found. Click Refresh Models."
        :loading="upstreamModelsLoading"
        multiple
        @open="onComboboxOpen"
        @update:model-value="(v) => emit('update:enabledModels', v as string[])"
      />
    </div>

    <!-- Custom model input -->
    <div class="flex items-center gap-2">
      <Input
        :model-value="customModel"
        placeholder="Or type a model name..."
        class="flex-1"
        :disabled="disabled"
        @update:model-value="customModel = String($event)"
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

    <!-- Refresh + loading / error -->
    <div class="flex items-center gap-2">
      <Button
        type="button"
        variant="outline"
        size="sm"
        :disabled="disabled || upstreamModelsLoading"
        @click="handleManualRefresh"
      >
        <Loader2 v-if="upstreamModelsLoading" class="h-3.5 w-3.5 animate-spin" />
        <RefreshCw v-else class="h-3.5 w-3.5" />
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

    <!-- ── Quick Create Route Confirmation Dialog ──────────── -->
    <Dialog :open="routeToCreate !== null" @update:open="(v) => !v && (routeToCreate = null)">
      <DialogContent class="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Create Route</DialogTitle>
          <DialogDescription>
            Create a single-target route for
            <span class="text-foreground font-mono font-medium"
              >{{ providerName }}/{{ routeToCreate }}</span
            >.
          </DialogDescription>
        </DialogHeader>

        <div class="space-y-2 py-2">
          <FieldLabel for="quick-route-name">Route Name</FieldLabel>
          <Input
            id="quick-route-name"
            :model-value="customRouteName"
            placeholder="e.g. volc/deepseek-v4-flash"
            class="font-mono text-sm"
            @input="customRouteName = ($event.target as HTMLInputElement).value"
            @keydown.enter.prevent="canConfirmRoute && confirmCreateRoute()"
          />
          <p v-if="isRouteExisting" class="text-destructive text-xs">
            Route "{{ customRouteName.trim() }}" already exists.
          </p>
        </div>

        <DialogFooter class="flex flex-row justify-end gap-2 sm:justify-end">
          <Button
            variant="outline"
            size="sm"
            :disabled="creatingRoute"
            @click="routeToCreate = null"
          >
            Cancel
          </Button>
          <Button size="sm" :disabled="!canConfirmRoute" @click="confirmCreateRoute">
            <Loader2 v-if="creatingRoute" class="h-3 w-3 animate-spin" />
            {{ creatingRoute ? "Creating…" : "Confirm" }}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </div>
</template>

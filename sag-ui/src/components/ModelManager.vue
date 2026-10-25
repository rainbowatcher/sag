<script setup lang="ts">
import { GitBranch, Loader2, Plus, RefreshCw, Trash2, Zap } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import { toast } from "vue-sonner"
import Combobox from "@/components/Combobox.vue"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Item, ItemActions, ItemContent, ItemGroup, ItemTitle } from "@/components/ui/item"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { useResponsiveModal } from "@/composables/useResponsiveModal"
import { t } from "@/lib/i18n"
import { getErrorMessage } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { CreateRoutePayload } from "@/types/api"

const UNLABELED = "—"

function modelLabel(item: Record<string, unknown>): string {
  const raw = item.id ?? item.name ?? item.model ?? Object.values(item)[0]
  return typeof raw === "string" && raw !== "" ? raw : UNLABELED
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
    upstreamModels: Record<string, unknown>[]
    upstreamModelsLoading: boolean
    upstreamModelsError: string
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
  fetchModels: []
  testModel: [model: string]
}>()

const routeStore = useRouteStore()

// Quick Create Route confirmation: Dialog on desktop, bottom Drawer on mobile.
const { Modal } = useResponsiveModal()

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
    void routeStore.fetchRoutes()
  }
}

function onRouteCreateOpenChange(v: boolean) {
  if (!v) routeToCreate.value = null
}

async function confirmCreateRoute() {
  if (!props.providerName || !routeToCreate.value) return
  const routeName = customRouteName.value.trim()
  if (!routeName || isRouteExisting.value) return

  creatingRoute.value = true
  try {
    const payload: CreateRoutePayload = {
      model_name: routeName,
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
    toast.success(t("routes.toast.created", { name: routeName }))
    routeToCreate.value = null
    await routeStore.fetchRoutes()
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("routes.toast.createFailed")))
  } finally {
    creatingRoute.value = false
  }
}

// ── Internal state ─────────────────────────────────
const customModel = ref("")

const upstreamModelNames = computed(
  () =>
    new Set(props.upstreamModels.map((m) => modelLabel(m)).filter((label) => label !== UNLABELED)),
)
const hasFetchedUpstream = computed(() => props.upstreamModels.length > 0)

function isModelMissingFromUpstream(model: string): boolean {
  return hasFetchedUpstream.value && !upstreamModelNames.value.has(model)
}

const hasFetched = ref(false)

watch(
  () => props.upstreamModels,
  (models) => {
    if (models.length > 0) hasFetched.value = true
  },
)

const comboboxOptions = computed(() => {
  const upstreamList: { value: string; label: string }[] = []
  for (const m of props.upstreamModels) {
    const val = modelLabel(m)
    // Skip unlabeled placeholders — they must not become selectable keys.
    if (val === UNLABELED) continue
    upstreamList.push({ value: val, label: val })
  }
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
    emit("fetchModels")
  }
}

function handleManualRefresh() {
  hasFetched.value = true
  emit("fetchModels")
}

function addCustomModel() {
  const trimmed = customModel.value.trim()
  if (trimmed && !props.enabledModels.includes(trimmed)) {
    emit("update:enabledModels", [...props.enabledModels, trimmed])
  }
  customModel.value = ""
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
      {{ $t("providers.models.noRestriction") }}
    </p>

    <ItemGroup v-if="enabledModels.length > 0" class="overflow-hidden rounded-md border">
      <Item
        v-for="model in enabledModels"
        :key="model"
        size="sm"
        class="border-border/40 flex-nowrap rounded-none border-b px-2 py-0.5 last:border-b-0"
        :class="{
          'text-muted-foreground bg-amber-500/10 dark:bg-amber-500/15':
            isModelMissingFromUpstream(model),
        }"
      >
        <ItemContent class="min-w-0 flex-1 flex-row items-center gap-2">
          <ItemTitle
            class="min-w-0 flex-1 truncate font-mono text-sm"
            :class="{
              'font-medium text-amber-700 dark:text-amber-300': isModelMissingFromUpstream(model),
            }"
          >
            {{ model }}
          </ItemTitle>
        </ItemContent>

        <ItemActions class="flex-none gap-1">
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
              <template v-else> {{ $t("providers.models.clickToTest") }} </template>
            </TooltipContent>
          </Tooltip>

          <Tooltip v-if="providerName">
            <TooltipTrigger as-child>
              <Button
                variant="ghost"
                size="icon-sm"
                :title="$t('providers.models.createRouteTitle')"
                :aria-label="$t('providers.models.createRouteTitle')"
                @click.stop="openCreateRoute(model)"
              >
                <GitBranch class="h-3.5 w-3.5" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="bottom" align="end" class="text-xs">
              {{ $t("providers.models.createRouteHint", { provider: providerName, model }) }}
            </TooltipContent>
          </Tooltip>

          <Button variant="ghost" size="icon-sm" @click="removeModel(model)">
            <Trash2 class="text-destructive h-3 w-3" />
          </Button>
        </ItemActions>
      </Item>
    </ItemGroup>

    <!-- Multi-select model combobox -->
    <div class="min-w-0">
      <Combobox
        :model-value="enabledModels"
        :options="comboboxOptions"
        :placeholder="$t('providers.models.selectPlaceholder')"
        :search-placeholder="$t('providers.models.searchPlaceholder')"
        :empty-text="$t('providers.models.emptyText')"
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
        :placeholder="$t('providers.models.customPlaceholder')"
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
        {{ $t("providers.models.add") }}
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
        {{ $t("providers.models.refresh") }}
      </Button>
      <div v-if="upstreamModelsLoading" class="text-muted-foreground text-xs">
        {{ $t("providers.models.loadingModels") }}
      </div>
    </div>

    <div v-if="upstreamModelsError" class="rounded-md bg-red-50 px-2 py-1 text-xs text-red-500">
      {{ upstreamModelsError }}
    </div>

    <!-- ── Quick Create Route Confirmation Dialog / Drawer ──── -->
    <component
      :is="Modal.Root"
      :open="routeToCreate !== null"
      @update:open="onRouteCreateOpenChange"
    >
      <component :is="Modal.Content" class="sm:max-w-md">
        <component :is="Modal.Header">
          <component :is="Modal.Title">{{
            $t("providers.models.createRouteDialogTitle")
          }}</component>
          <component :is="Modal.Description">
            {{ $t("providers.models.createRouteDescLead")
            }}<span class="text-foreground font-mono font-medium"
              >{{ providerName }}/{{ routeToCreate }}</span
            >{{ $t("providers.models.createRouteDescTail") }}
          </component>
        </component>

        <div class="space-y-2 px-4 py-2 sm:px-0">
          <FieldLabel for="quick-route-name">{{
            $t("providers.models.routeNameLabel")
          }}</FieldLabel>
          <Input
            id="quick-route-name"
            :model-value="customRouteName"
            :placeholder="$t('providers.models.routeNamePlaceholder')"
            class="font-mono text-sm"
            @input="customRouteName = ($event.target as HTMLInputElement).value"
            @keydown.enter.prevent="canConfirmRoute && confirmCreateRoute()"
          />
          <p v-if="isRouteExisting" class="text-destructive text-xs">
            {{ $t("providers.models.routeExists", { name: customRouteName.trim() }) }}
          </p>
        </div>

        <component :is="Modal.Footer">
          <Button
            variant="outline"
            size="sm"
            :disabled="creatingRoute"
            @click="routeToCreate = null"
          >
            {{ $t("common.cancel") }}
          </Button>
          <Button size="sm" :disabled="!canConfirmRoute" @click="confirmCreateRoute">
            <Loader2 v-if="creatingRoute" class="h-3 w-3 animate-spin" />
            {{ creatingRoute ? $t("providers.models.creating") : $t("common.confirm") }}
          </Button>
        </component>
      </component>
    </component>
  </div>
</template>

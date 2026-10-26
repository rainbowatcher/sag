<script setup lang="ts">
import { Check, Info, Loader2, Search } from "@lucide/vue"
import type { ComponentPublicInstance } from "vue"
import { computed, nextTick, onBeforeUnmount, ref, watch } from "vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { useResponsiveModal } from "@/composables/useResponsiveModal"
import { t } from "@/lib/i18n"
import {
  applyPickerSelection,
  buildPickerGroups,
  diffSelection,
  filterPickerGroups,
  flattenOptions,
  selectedKeysFromTargets,
} from "@/lib/targetPicker"
import { cn } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { Provider, TargetItem } from "@/types/api"

const props = defineProps<{
  open: boolean
  targets: TargetItem[]
  providers: Provider[]
  providerModels: Record<string, string[]>
  /** Id to give the first appended row (the editor's `max(ids) + 1`). */
  nextId: number
}>()

const emit = defineEmits<{
  "update:open": [value: boolean]
  confirm: [targets: TargetItem[]]
}>()

const routeStore = useRouteStore()
const { Modal } = useResponsiveModal()

const query = ref("")
const selected = ref<Set<string>>(new Set())
const loading = ref(false)
const activeIndex = ref(0)
const searchBox = ref<HTMLDivElement | null>(null)
/** Option key → row element, for keeping the active row in view. */
const optionEls = new Map<string, HTMLElement>()

// ── List model ────────────────────────────────────────────────────
const groups = computed(() =>
  buildPickerGroups(props.providers, props.providerModels, props.targets),
)
const visibleGroups = computed(() => filterPickerGroups(groups.value, query.value))
/** Flat list order = the keyboard navigation order. */
const flatOptions = computed(() => flattenOptions(visibleGroups.value))

const optionIndex = computed(() => {
  const index = new Map<string, number>()
  flatOptions.value.forEach((option, i) => index.set(option.key, i))
  return index
})

const activeKey = computed(() => flatOptions.value[activeIndex.value]?.key ?? "")
/**
 * The highlighted row can disappear while filtering; never point
 * `aria-activedescendant` at an option that is no longer rendered.
 */
const activeDescendant = computed(() => {
  const key = activeKey.value
  if (!key || !optionIndex.value.has(key)) return undefined
  return optionId(key)
})

function optionId(key: string): string {
  return `target-picker-option-${optionIndex.value.get(key) ?? 0}`
}

function isSelected(key: string): boolean {
  return selected.value.has(key)
}

function toggle(key: string) {
  const next = new Set(selected.value)
  if (next.has(key)) next.delete(key)
  else next.add(key)
  selected.value = next
}

function setActive(key: string) {
  activeIndex.value = optionIndex.value.get(key) ?? 0
}

function registerOption(key: string, el: Element | ComponentPublicInstance | null) {
  if (el instanceof HTMLElement) optionEls.set(key, el)
  else optionEls.delete(key)
}

// ── Set difference → confirm summary ──────────────────────────────
const diff = computed(() => diffSelection(props.targets, [...selected.value], groups.value))

const confirmLabel = computed(() => {
  const added = diff.value.added.length
  const removed = diff.value.removed.length
  if (added > 0 && removed > 0) {
    return t("routes.targetPicker.confirmBoth", { add: added, remove: removed })
  }
  if (added > 0) return t("routes.targetPicker.confirmAdd", { count: added })
  if (removed > 0) return t("routes.targetPicker.confirmRemove", { count: removed })
  return t("common.close")
})

// ── Model loading ─────────────────────────────────────────────────
const failedProviders = computed(() =>
  props.providers.filter((p) => routeStore.providerModelsErrors[p.name]).map((p) => p.name),
)

const errorText = computed(() =>
  failedProviders.value.length > 0
    ? t("routes.targetPicker.loadError", { providers: failedProviders.value.join(", ") })
    : "",
)

/**
 * Load every provider's model list. `/api/providers/{name}/models` only
 * echoes the configured `enabled_models` (no upstream probe), so a provider
 * with an empty list stays empty — the picker still renders its section with
 * an empty-state hint. Cached lists are reused so reopening the picker does
 * not re-hit the API.
 */
async function loadModels() {
  if (props.providers.length === 0) return
  loading.value = true
  try {
    await Promise.all(
      props.providers.map(async (provider) => {
        if ((props.providerModels[provider.name]?.length ?? 0) > 0) return
        if (provider.enabled_models.length > 0) {
          await routeStore.loadProviderModels(provider.name, props.providers)
          return
        }
        await routeStore.fetchProviderModels(provider.name)
      }),
    )
  } finally {
    loading.value = false
  }
}

// ── Open / close ──────────────────────────────────────────────────
function onOpenChange(value: boolean) {
  emit("update:open", value)
}

function close() {
  emit("update:open", false)
}

function confirm() {
  const { added, removed } = diff.value
  if (added.length === 0 && removed.length === 0) {
    close()
    return
  }
  emit(
    "confirm",
    applyPickerSelection(props.targets, [...selected.value], groups.value, props.nextId),
  )
  close()
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    if (flatOptions.value.length === 0) return
    event.preventDefault()
    const delta = event.key === "ArrowDown" ? 1 : -1
    const last = flatOptions.value.length - 1
    activeIndex.value = Math.min(last, Math.max(0, activeIndex.value + delta))
    return
  }
  if (event.key === "Enter" || event.key === " ") {
    // Space is the documented toggle key (0006). It cannot double as a typed
    // space here: the filter is a single substring match, so a literal space in
    // the needle gains nothing, while `preventDefault` keeps the page/list from
    // scrolling under the dialog.
    const option = flatOptions.value[activeIndex.value]
    if (!option || event.isComposing) return
    event.preventDefault()
    toggle(option.key)
  }
  // Escape is left to the dialog/drawer primitive, which closes without applying.
}

watch(
  () => props.open,
  (open) => {
    if (!open) {
      // The option rows are going away; drop their refs so a later open cannot
      // scroll a detached element.
      optionEls.clear()
      return
    }
    query.value = ""
    selected.value = new Set(selectedKeysFromTargets(props.targets))
    activeIndex.value = 0
    void loadModels()
    nextTick(() => searchBox.value?.querySelector("input")?.focus())
  },
  { immediate: true },
)

watch(query, () => {
  activeIndex.value = 0
})

watch(flatOptions, (list, previous) => {
  const previousKey = previous[activeIndex.value]?.key
  if (previousKey !== undefined && list[activeIndex.value]?.key !== previousKey) {
    // Filtering dropped the highlighted row: re-anchor on the first visible
    // option instead of leaving the highlight (and `aria-activedescendant`) on
    // a row that is no longer rendered.
    activeIndex.value = 0
  }
  if (activeIndex.value > list.length - 1) {
    activeIndex.value = Math.max(0, list.length - 1)
  }
})

watch(activeKey, async (key) => {
  if (!key) return
  await nextTick()
  optionEls.get(key)?.scrollIntoView({ block: "nearest" })
})

onBeforeUnmount(() => {
  optionEls.clear()
})
</script>

<template>
  <component :is="Modal.Root" :open="open" @update:open="onOpenChange">
    <component
      :is="Modal.Content"
      class="flex max-h-[85vh] flex-col gap-3 overflow-hidden p-0 sm:max-w-2xl"
    >
      <component :is="Modal.Header" class="px-4 pt-4">
        <component :is="Modal.Title" class="flex items-center gap-1.5">
          {{ $t("routes.targetPicker.title") }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              {{ $t("routes.targetPicker.description") }}
            </TooltipContent>
          </Tooltip>
        </component>
        <component :is="Modal.Description" class="sr-only">
          {{ $t("routes.targetPicker.description") }}
        </component>
      </component>

      <!-- Search + selection count -->
      <div class="space-y-1.5 px-4">
        <div ref="searchBox" class="relative">
          <Search
            class="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2"
          />
          <Input
            v-model="query"
            class="pl-8"
            :placeholder="$t('routes.targetPicker.search')"
            role="combobox"
            aria-controls="target-picker-listbox"
            :aria-activedescendant="activeDescendant"
            aria-autocomplete="list"
            aria-expanded="true"
            :aria-label="$t('routes.targetPicker.search')"
            @keydown="onKeydown"
          />
        </div>
        <div class="text-muted-foreground flex items-center justify-between text-xs">
          <span>{{ $t("routes.targetPicker.selectedCount", { count: selected.size }) }}</span>
          <span v-if="loading" class="inline-flex items-center gap-1">
            <Loader2 class="size-3 animate-spin" />
            {{ $t("routes.targetPicker.loading") }}
          </span>
        </div>
      </div>

      <div
        v-if="errorText"
        class="mx-4 rounded-md bg-red-50 px-2 py-1 text-xs text-red-500 dark:bg-red-950/40"
      >
        {{ errorText }}
      </div>

      <!-- Provider-grouped, multi-select list -->
      <div
        id="target-picker-listbox"
        role="listbox"
        aria-multiselectable="true"
        :aria-label="$t('routes.targetPicker.title')"
        class="max-h-[min(55vh,24rem)] overflow-y-auto border-t px-2 py-1"
      >
        <div
          v-if="visibleGroups.length === 0"
          class="text-muted-foreground py-10 text-center text-sm"
        >
          {{ query.trim() ? $t("routes.targetPicker.empty") : $t("routes.targetPicker.noModels") }}
        </div>

        <div v-for="group in visibleGroups" :key="group.provider" class="mb-1">
          <!-- Group headings are hidden while filtering, like magpie's picker -->
          <div
            v-if="!query.trim()"
            class="text-muted-foreground flex items-center gap-1.5 px-2 py-1.5 text-xs font-medium"
          >
            <span class="truncate">{{ group.provider }}</span>
            <Badge v-if="group.disabledProvider" variant="outline" class="shrink-0 text-[10px]">
              {{ $t("routes.targetPicker.providerDisabled") }}
            </Badge>
          </div>

          <!--
            A provider without models keeps its section so it stays discoverable
            instead of silently vanishing; the hint waits for loading to settle so
            it never reads as a result before the models arrive.
          -->
          <div
            v-if="group.options.length === 0 && !loading"
            class="text-muted-foreground px-2 py-1.5 text-xs"
          >
            {{ $t("routes.targetPicker.providerNoModels") }}
          </div>

          <div
            v-for="option in group.options"
            :id="optionId(option.key)"
            :key="option.key"
            :ref="
              (el) => registerOption(option.key, el as Element | ComponentPublicInstance | null)
            "
            role="option"
            :aria-selected="isSelected(option.key)"
            :class="
              cn(
                'flex cursor-pointer items-center gap-2 rounded-sm px-2 py-1.5 text-sm select-none',
                activeKey === option.key && 'bg-accent text-accent-foreground',
                option.disabledProvider && 'text-muted-foreground',
              )
            "
            @click="toggle(option.key)"
            @mouseenter="setActive(option.key)"
          >
            <Check
              :class="cn('size-4 shrink-0', isSelected(option.key) ? 'opacity-100' : 'opacity-0')"
              aria-hidden="true"
            />
            <span class="min-w-0 flex-1 truncate font-mono">{{ option.model }}</span>
            <!-- Provider stays visible while filtering hides the group headings -->
            <span class="text-muted-foreground max-w-[150px] shrink-0 truncate text-xs">
              {{ option.provider }}
            </span>
            <span v-if="option.orphan" class="shrink-0 text-xs text-amber-600 dark:text-amber-400">
              {{ $t("routes.targetPicker.orphan") }}
            </span>
          </div>
        </div>
      </div>

      <component :is="Modal.Footer" class="flex-row justify-end px-4 pb-4">
        <Button variant="outline" size="sm" type="button" @click="close">
          {{ $t("common.cancel") }}
        </Button>
        <Button size="sm" type="button" @click="confirm">{{ confirmLabel }}</Button>
      </component>
    </component>
  </component>
</template>

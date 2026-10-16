<script setup lang="ts">
import { Copy, Loader2, Pen, Plus, Search, Trash2 } from "@lucide/vue"
import { FlexRender, createColumnHelper, getCoreRowModel, useVueTable } from "@tanstack/vue-table"
import { computed, h, onMounted, ref, watch } from "vue"
import { toast } from "vue-sonner"
import ProviderAddForm from "@/components/providers/ProviderAddForm.vue"
import ProviderEditForm from "@/components/providers/ProviderEditForm.vue"
import RefreshButton from "@/components/RefreshButton.vue"
import { Badge } from "@/components/ui/badge"
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
import { Switch } from "@/components/ui/switch"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip"
import { useResponsiveModal } from "@/composables/useResponsiveModal"
import { t } from "@/lib/i18n"
import { getErrorMessage } from "@/lib/utils"
import { useCircuitBreakerStore } from "@/stores/circuitBreaker"
import { useProviderStore } from "@/stores/providers"
import type { AddProviderPayload, ApiKind, CircuitBreakerEntry, Provider } from "@/types/api"

const store = useProviderStore()
const cbStore = useCircuitBreakerStore()

// ── Circuit breaker aggregation ────────────────────────────────────

/// Breaker entries grouped by provider name.
interface BreakerGroup {
  open: number
  halfOpen: number
  entries: CircuitBreakerEntry[]
}

const breakersByProvider = computed(() => {
  const map: Record<string, BreakerGroup> = {}
  for (const b of cbStore.breakers) {
    const entry = (map[b.provider] ??= { open: 0, halfOpen: 0, entries: [] })
    if (b.state === "open") entry.open++
    else entry.halfOpen++
    entry.entries.push(b)
  }
  return map
})

// ── Filters ───────────────────────────────────────────────────────
const searchInput = ref("")
const filterApi = ref<"all" | ApiKind>("all")
const sortByName = ref(true)

// Client-side filter: split search by spaces, all keywords must match a model in enabled_models.
const filteredProviders = computed(() => {
  if (!searchInput.value.trim()) return store.providers
  const keywords = searchInput.value.trim().toLowerCase().split(/\s+/)
  return store.providers.filter((p) =>
    keywords.every((kw) => p.enabled_models.some((m) => m.toLowerCase().includes(kw))),
  )
})

// Add form
const showAdd = ref(false)

// Edit
const editingProvider = ref<Provider | null>(null)

// Delete
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)

// Toggle loading state (prevents UI flicker during enable/disable)
const togglingProvider = ref<string | null>(null)

// ── Data fetching ─────────────────────────────────────────────────
async function fetchProviders() {
  await store.fetchProviders({
    apis: filterApi.value === "all" ? undefined : filterApi.value,
    sort: sortByName.value ? "name" : undefined,
  })
}

watch([filterApi, sortByName], () => {
  fetchProviders()
})

onMounted(async () => {
  await Promise.all([fetchProviders(), cbStore.fetchBreakers()])
})

async function refreshAll() {
  await Promise.all([fetchProviders(), cbStore.fetchBreakers()])
}

// ── Enable / Disable toggle ───────────────────────────────────────
async function toggleEnabled(provider: Provider) {
  const newState = !provider.enabled
  togglingProvider.value = provider.name
  try {
    await store.updateProvider(provider.name, { enabled: newState })
    toast.success(
      newState
        ? t("providers.toast.enabled", { name: provider.name })
        : t("providers.toast.disabled", { name: provider.name }),
    )
  } catch (e: any) {
    const msg = getErrorMessage(e)
    toast.error(msg)
  } finally {
    togglingProvider.value = null
    await fetchProviders()
  }
}

// ── Add flow ──────────────────────────────────────────────────────
async function onCreated() {
  showAdd.value = false
  await fetchProviders()
}

// ── Edit flow ─────────────────────────────────────────────────────
function startEdit(p: Provider) {
  confirmingDelete.value = null
  editingProvider.value = p
}

function onEditClose() {
  editingProvider.value = null
}

async function onUpdated() {
  editingProvider.value = null
  await fetchProviders()
}

// ── Copy flow ─────────────────────────────────────────────────────
// Dialog on desktop / bottom Drawer on mobile — same dual-track modal the
// provider forms and ModelManager's quick-create use.
const { Modal } = useResponsiveModal()

const copyingProvider = ref<Provider | null>(null)
const copyName = ref("")
const isCopying = ref(false)

const isCopyNameEmpty = computed(() => !copyName.value.trim())

const isCopyNameExisting = computed(() => {
  const name = copyName.value.trim()
  if (!name) return false
  return store.providers.some((p) => p.name === name)
})

const canConfirmCopy = computed(
  () => !isCopyNameEmpty.value && !isCopyNameExisting.value && !isCopying.value,
)

function startCopy(p: Provider) {
  confirmingDelete.value = null
  copyName.value = `${p.name}-copy`
  copyingProvider.value = p
}

function onCopyOpenChange(v: boolean) {
  if (!v) copyingProvider.value = null
}

/**
 * JSON round-trip clone. `structuredClone` cannot clone Vue's reactive
 * proxies, and the store hands us reactive `Provider` objects.
 */
function deepClone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T
}

async function confirmCopy() {
  const source = copyingProvider.value
  if (!source) return
  const newName = copyName.value.trim()
  if (!newName || isCopyNameExisting.value) return

  isCopying.value = true
  try {
    const payload: AddProviderPayload = {
      name: newName,
      api_keys: [...source.api_keys],
      key_strategy: source.key_strategy,
      apis: deepClone(source.apis),
      ...(source.fallback ? { fallback: source.fallback } : {}),
      timeout_secs: source.timeout_secs,
      headers: Object.keys(source.headers).length > 0 ? { ...source.headers } : undefined,
      body_overrides:
        Object.keys(source.body_overrides).length > 0
          ? deepClone(source.body_overrides)
          : undefined,
      strip_fields: source.strip_fields.length > 0 ? [...source.strip_fields] : undefined,
      model_strip_fields:
        Object.keys(source.model_strip_fields).length > 0
          ? deepClone(source.model_strip_fields)
          : undefined,
      model_overrides:
        Object.keys(source.model_overrides).length > 0
          ? deepClone(source.model_overrides)
          : undefined,
      role_mappings:
        Object.keys(source.role_mappings).length > 0 ? { ...source.role_mappings } : undefined,
      enabled_models: [...source.enabled_models],
      proxy_mode: source.proxy_mode,
    }

    await store.addProvider(payload)
    copyingProvider.value = null
    toast.success(t("providers.toast.created", { name: newName }))

    // addProvider refreshed the list, so the new provider is present here.
    const created = store.providers.find((p) => p.name === newName)
    if (created) editingProvider.value = created
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("providers.toast.copyFailed")))
  } finally {
    isCopying.value = false
  }
}

// ── Delete flow ───────────────────────────────────────────────────
function startDelete(name: string) {
  confirmingDelete.value = confirmingDelete.value === name ? null : name
}

async function confirmDelete(name: string) {
  deleting.value = true
  try {
    await store.deleteProvider(name)
    toast.success(t("providers.toast.deleted", { name }))
    confirmingDelete.value = null
    await fetchProviders()
  } catch (e: any) {
    const msg = getErrorMessage(e)
    toast.error(msg)
  } finally {
    deleting.value = false
  }
}

// ══════════════════════════════════════════════════════════════════
// TanStack Table
// ══════════════════════════════════════════════════════════════════

const columnHelper = createColumnHelper<Provider>()

// headers must stay functions: () => t(...) — string headers freeze on locale switch (see review-p1-p2 Minor-2)
const columns = [
  columnHelper.accessor("name", {
    header: () => t("common.name"),
    cell: (info) => info.getValue(),
  }),
  columnHelper.display({
    id: "apis",
    header: () => t("providerList.columns.apis"),
    cell: (info) => {
      const p = info.row.original
      const kinds = Object.keys(p.apis ?? {}) as ApiKind[]
      if (kinds.length === 0) {
        return h("span", { class: "text-muted-foreground text-xs" }, "—")
      }
      const fallbackNote = p.fallback
        ? t("providerList.fallbackNamed", { name: p.fallback })
        : t("providerList.fallbackAuto")
      return h(
        "div",
        { class: "flex flex-wrap gap-1", title: fallbackNote },
        kinds.map((kind) =>
          h(
            Badge,
            {
              variant: "outline",
              class: "font-mono text-[10px] uppercase",
            },
            () => kind,
          ),
        ),
      )
    },
    meta: { class: "min-w-[160px]" },
  }),
  columnHelper.accessor("api_keys", {
    header: () => t("providerList.columns.keys"),
    cell: (info) => h("span", { class: "tabular-nums" }, String(info.getValue().length)),
    meta: { class: "text-center" },
  }),
  columnHelper.accessor("enabled", {
    header: () => t("common.status"),
    cell: (info) =>
      h(
        Badge,
        {
          variant: info.getValue() ? "default" : "secondary",
          class: "text-[10px]",
        },
        () => (info.getValue() ? t("common.enabled") : t("common.disabled")),
      ),
    meta: { class: "text-left" },
  }),
  columnHelper.display({
    id: "breakers",
    header: () => t("providerList.columns.cb"),
    cell: (info) => {
      const provider = info.row.original
      const counts = breakersByProvider.value[provider.name]

      // Healthy: green badge.
      if (!counts) {
        return h(Tooltip, null, () => [
          h(TooltipTrigger, { asChild: true }, () =>
            h("div", { class: "flex items-center gap-1" }, [
              h(
                Badge,
                {
                  variant: "outline",
                  class: "text-[10px] px-1.5 border-green-500/30 bg-green-500/10 text-green-600",
                },
                () => "0",
              ),
            ]),
          ),
          h(TooltipContent, { side: "left", class: "max-w-xs text-xs" }, () =>
            h(
              "pre",
              { class: "whitespace-pre-wrap font-mono text-[11px]" },
              t("providerList.allTargetsHealthy"),
            ),
          ),
        ])
      }

      // Tripped: amber/red badge with per-target breakdown.
      const openList = counts.entries.filter((b) => b.state === "open")
      const halfOpenList = counts.entries.filter((b) => b.state === "half_open")

      const tooltipRows: string[] = []
      if (openList.length > 0) {
        tooltipRows.push(t("providerList.breakerOpenGroup", { count: openList.length }))
        for (const b of openList) tooltipRows.push(`  ${b.model} → ${b.real_model}`)
      }
      if (halfOpenList.length > 0) {
        tooltipRows.push(t("providerList.breakerHalfOpenGroup", { count: halfOpenList.length }))
        for (const b of halfOpenList) tooltipRows.push(`  ${b.model} → ${b.real_model}`)
      }

      return h(Tooltip, null, () => [
        h(TooltipTrigger, { asChild: true }, () =>
          h("div", { class: "flex items-center gap-1" }, [
            h(Badge, { variant: "destructive", class: "text-[10px] px-1.5" }, () =>
              String(counts.open + counts.halfOpen),
            ),
          ]),
        ),
        h(TooltipContent, { side: "left", class: "max-w-xs text-xs" }, () =>
          h("pre", { class: "whitespace-pre-wrap font-mono text-[11px]" }, tooltipRows.join("\n")),
        ),
      ])
    },
    meta: { class: "text-center w-10" },
  }),
  columnHelper.display({
    id: "actions",
    header: "",
    cell: (info) => {
      const provider = info.row.original
      return h("div", { class: "flex items-center justify-end gap-1" }, [
        h(Switch, {
          modelValue: provider.enabled,
          disabled: togglingProvider.value === provider.name,
          "onUpdate:modelValue": () => toggleEnabled(provider),
          title: provider.enabled
            ? t("providerList.disableProvider")
            : t("providerList.enableProvider"),
          "aria-label": provider.enabled
            ? t("providerList.disableProvider")
            : t("providerList.enableProvider"),
        }),
        h("span", { class: "bg-border mx-1 h-4 w-px shrink-0", "aria-hidden": "true" }),
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            onClick: () => startEdit(info.row.original),
            title: t("providerList.editProvider"),
            "aria-label": t("providerList.editProvider"),
          },
          () => h(Pen, { class: "h-3.5 w-3.5" }),
        ),
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            onClick: () => startCopy(info.row.original),
            title: t("providerList.copyProvider"),
            "aria-label": t("providerList.copyProvider"),
          },
          () => h(Copy, { class: "h-3.5 w-3.5" }),
        ),
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            class: confirmingDelete.value === info.row.original.name ? "text-destructive" : "",
            onClick: () => startDelete(info.row.original.name),
            title:
              confirmingDelete.value === info.row.original.name
                ? t("providerList.cancelDelete")
                : t("providerList.deleteProvider"),
            "aria-label":
              confirmingDelete.value === info.row.original.name
                ? t("providerList.cancelDelete")
                : t("providerList.deleteProvider"),
          },
          () => h(Trash2, { class: "h-3.5 w-3.5" }),
        ),
      ])
    },
    meta: { class: "text-right" },
  }),
]

const table = useVueTable({
  get data() {
    return filteredProviders.value
  },
  columns,
  getCoreRowModel: getCoreRowModel(),
})
</script>

<template>
  <div class="flex h-full flex-col space-y-4">
    <!-- ── Header ────────────────────────────────────────────────── -->
    <div class="shrink-0 space-y-4">
      <div class="flex flex-wrap items-center gap-2">
        <div class="min-w-0 flex-1">
          <div class="relative">
            <Search
              class="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 z-10 h-4 w-4 -translate-y-1/2"
            />
            <Input
              v-model="searchInput"
              type="text"
              :placeholder="$t('providers.list.searchPlaceholder')"
              class="h-8 pl-8"
            />
          </div>
        </div>
        <div class="hidden md:block">
          <Select
            :model-value="filterApi"
            @update:model-value="(value) => (filterApi = value as 'all' | ApiKind)"
          >
            <SelectTrigger class="h-8 w-[220px]">
              <SelectValue>{{
                filterApi === "all" ? $t("providers.list.filterAllApis") : filterApi
              }}</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">{{ $t("providers.list.filterAllApis") }}</SelectItem>
              <SelectItem value="chat_completions">chat_completions</SelectItem>
              <SelectItem value="responses">responses</SelectItem>
              <SelectItem value="anthropic_messages">anthropic_messages</SelectItem>
              <SelectItem value="image">image</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div class="hidden md:block">
          <Select
            :model-value="sortByName ? 'name' : 'default'"
            @update:model-value="(value) => (sortByName = value === 'name')"
          >
            <SelectTrigger class="h-8 w-[160px]">
              <SelectValue>{{
                sortByName ? $t("providers.list.sortName") : $t("common.default")
              }}</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="name">{{ $t("providers.list.sortName") }}</SelectItem>
              <SelectItem value="default">{{ $t("common.default") }}</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <RefreshButton :loading="store.providersLoading" @click="refreshAll()" />
          <Button size="sm" @click="showAdd = true">
            <Plus class="h-3.5 w-3.5" />
            {{ $t("providers.list.addProvider") }}
          </Button>
        </div>
      </div>
    </div>

    <!-- ── Add Drawer / Dialog ───────────────────────────────────── -->
    <ProviderAddForm v-if="showAdd" :open="showAdd" @close="showAdd = false" @created="onCreated" />

    <!-- ── Loading ───────────────────────────────────────────────── -->
    <div
      v-if="store.providersLoading && store.providers.length === 0"
      class="text-muted-foreground py-12 text-center"
    >
      <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
      {{ $t("common.loading") }}
    </div>

    <!-- ── Empty ─────────────────────────────────────────────────── -->
    <div
      v-else-if="filteredProviders.length === 0"
      class="text-muted-foreground py-12 text-center text-sm"
    >
      {{
        searchInput || filterApi !== "all"
          ? $t("providers.list.noMatch")
          : $t("providers.list.empty")
      }}
    </div>

    <!-- ── Table ─────────────────────────────────────────────────── -->
    <div v-else class="min-h-0 flex-1">
      <div class="max-h-full overflow-auto rounded-lg border [&>div]:overflow-visible">
        <TooltipProvider :delay-duration="200">
          <Table>
            <TableHeader class="bg-background/95 sticky top-0 z-10 backdrop-blur-xs">
              <TableRow v-for="headerGroup in table.getHeaderGroups()" :key="headerGroup.id">
                <TableHead
                  v-for="header in headerGroup.headers"
                  :key="header.id"
                  :class="header.column.columnDef.meta?.class"
                >
                  <FlexRender
                    v-if="!header.isPlaceholder"
                    :render="header.column.columnDef.header!"
                    :props="header.getContext()"
                  />
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="row in table.getRowModel().rows" :key="row.id">
                <TableCell
                  v-for="cell in row.getVisibleCells()"
                  :key="cell.id"
                  :class="cell.column.columnDef.meta?.class"
                >
                  <FlexRender :render="cell.column.columnDef.cell!" :props="cell.getContext()" />
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </TooltipProvider>
      </div>
    </div>

    <!-- ── Delete confirmation ───────────────────────────────────── -->
    <div
      v-if="confirmingDelete"
      class="bg-destructive/5 border-destructive/20 fixed inset-x-0 bottom-0 z-50 shrink-0 border-t p-4 shadow-lg md:relative md:inset-auto md:rounded-lg md:border md:shadow-none"
    >
      <div class="flex items-center justify-between gap-4">
        <p class="text-sm">
          {{ $t("providers.list.deleteConfirmPrefix")
          }}<span class="font-semibold">{{ confirmingDelete }}</span
          >{{ $t("providers.list.deleteConfirmSuffix") }}
        </p>
        <div class="flex gap-2">
          <Button variant="outline" size="sm" @click="confirmingDelete = null">
            {{ $t("common.cancel") }}
          </Button>
          <Button
            variant="destructive"
            size="sm"
            :disabled="deleting"
            @click="confirmDelete(confirmingDelete)"
          >
            <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
            {{ deleting ? $t("providers.list.deleting") : $t("common.delete") }}
          </Button>
        </div>
      </div>
    </div>

    <!-- ── Copy Provider Drawer / Dialog ─────────────────────────── -->
    <component :is="Modal.Root" :open="copyingProvider !== null" @update:open="onCopyOpenChange">
      <component :is="Modal.Content" class="sm:max-w-md">
        <component :is="Modal.Header">
          <component :is="Modal.Title">{{ $t("providers.list.copyTitle") }}</component>
          <component :is="Modal.Description">
            {{ $t("providers.list.copyDescLead")
            }}<span class="text-foreground font-mono font-medium">{{ copyingProvider?.name }}</span
            >{{ $t("providers.list.copyDescTail") }}
          </component>
        </component>

        <div class="space-y-2 px-4 py-2 sm:px-0">
          <FieldLabel for="copy-provider-name">{{ $t("providers.list.copyNameLabel") }}</FieldLabel>
          <Input
            id="copy-provider-name"
            :model-value="copyName"
            :placeholder="$t('providers.list.copyNamePlaceholder')"
            class="font-mono text-sm"
            @input="copyName = ($event.target as HTMLInputElement).value"
            @keydown.enter.prevent="canConfirmCopy && confirmCopy()"
          />
          <p v-if="isCopyNameExisting" class="text-destructive text-xs">
            {{ $t("providers.list.copyExists", { name: copyName.trim() }) }}
          </p>
        </div>

        <component :is="Modal.Footer">
          <Button variant="outline" size="sm" :disabled="isCopying" @click="copyingProvider = null">
            {{ $t("common.cancel") }}
          </Button>
          <Button size="sm" :disabled="!canConfirmCopy" @click="confirmCopy">
            <Loader2 v-if="isCopying" class="h-3 w-3 animate-spin" />
            {{ isCopying ? $t("providers.list.copying") : $t("providers.list.copyAndEdit") }}
          </Button>
        </component>
      </component>
    </component>

    <!-- ── Edit Drawer / Dialog ──────────────────────────────────── -->
    <ProviderEditForm
      v-if="editingProvider"
      :provider="editingProvider"
      @close="onEditClose"
      @updated="onUpdated"
    />
  </div>
</template>

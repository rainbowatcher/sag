<script setup lang="ts">
import { Loader2, Pen, Plus, RefreshCw, Search, Trash2 } from "@lucide/vue"
import { FlexRender, createColumnHelper, getCoreRowModel, useVueTable } from "@tanstack/vue-table"
import { computed, h, onMounted, ref, watch } from "vue"
import { toast } from "vue-sonner"
import ProviderAddForm from "@/components/providers/ProviderAddForm.vue"
import ProviderEditForm from "@/components/providers/ProviderEditForm.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
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
import { getErrorMessage } from "@/lib/utils"
import { useCircuitBreakerStore } from "@/stores/circuitBreaker"
import { useProviderStore } from "@/stores/providers"
import type { CircuitBreakerEntry, Provider } from "@/types/api"

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
const filterProviderType = ref("all")
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

// ── Data fetching ─────────────────────────────────────────────────
async function fetchProviders() {
  await store.fetchProviders({
    provider_type: filterProviderType.value === "all" ? undefined : filterProviderType.value,
    sort: sortByName.value ? "name" : undefined,
  })
}

watch([filterProviderType, sortByName], () => {
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
  try {
    await store.updateProvider(provider.name, { enabled: newState })
    toast.success(`Provider "${provider.name}" ${newState ? "enabled" : "disabled"}`)
  } catch (e: any) {
    const msg = getErrorMessage(e)
    toast.error(msg)
  } finally {
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

// ── Delete flow ───────────────────────────────────────────────────
function startDelete(name: string) {
  confirmingDelete.value = confirmingDelete.value === name ? null : name
}

async function confirmDelete(name: string) {
  deleting.value = true
  try {
    await store.deleteProvider(name)
    toast.success(`Provider "${name}" deleted`)
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

const columns = [
  columnHelper.accessor("name", {
    header: "Name",
    cell: (info) => info.getValue(),
  }),
  columnHelper.accessor("base_url", {
    header: "Base URL",
    cell: (info) =>
      h(
        "span",
        { class: "font-mono text-xs text-muted-foreground", title: info.getValue() },
        info.getValue(),
      ),
    meta: { class: "min-w-[200px] max-w-[12rem] overflow-hidden truncate" },
  }),
  columnHelper.accessor("models_endpoint", {
    header: "Models Endpoint",
    cell: (info) =>
      h("span", { class: "font-mono text-xs text-muted-foreground" }, info.getValue()),
    meta: { class: "min-w-[80px]" },
  }),
  columnHelper.accessor("provider_type", {
    header: "Type",
    cell: (info) =>
      h(Badge, { variant: "outline", class: "uppercase text-xs" }, () => info.getValue()),
  }),
  columnHelper.accessor("api_keys", {
    header: "Keys",
    cell: (info) => h("span", { class: "tabular-nums" }, String(info.getValue().length)),
    meta: { class: "text-center" },
  }),
  columnHelper.accessor("enabled", {
    header: "Status",
    cell: (info) => {
      const provider = info.row.original
      return h(Switch, {
        modelValue: info.getValue(),
        "onUpdate:modelValue": () => toggleEnabled(provider),
      })
    },
    meta: { class: "text-center" },
  }),
  columnHelper.display({
    id: "breakers",
    header: "CB",
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
            h("pre", { class: "whitespace-pre-wrap font-mono text-[11px]" }, "All targets healthy"),
          ),
        ])
      }

      // Tripped: amber/red badge with per-target breakdown.
      const openList = counts.entries.filter((b) => b.state === "open")
      const halfOpenList = counts.entries.filter((b) => b.state === "half_open")

      const tooltipRows: string[] = []
      if (openList.length > 0) {
        tooltipRows.push(`Open (${openList.length}):`)
        for (const b of openList) tooltipRows.push(`  ${b.model} → ${b.real_model}`)
      }
      if (halfOpenList.length > 0) {
        tooltipRows.push(`Half-Open (${halfOpenList.length}):`)
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
    cell: (info) =>
      h("div", { class: "flex items-center justify-end gap-1" }, [
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            onClick: () => startEdit(info.row.original),
            title: "Edit provider",
            "aria-label": "Edit provider",
          },
          () => h(Pen, { class: "h-3.5 w-3.5" }),
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
                ? "Cancel delete"
                : "Delete provider",
            "aria-label":
              confirmingDelete.value === info.row.original.name
                ? "Cancel delete"
                : "Delete provider",
          },
          () => h(Trash2, { class: "h-3.5 w-3.5" }),
        ),
      ]),
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
  <div class="flex h-full flex-col space-y-6">
    <!-- ── Header ────────────────────────────────────────────────── -->
    <div class="shrink-0 space-y-6">
      <div class="flex items-center justify-between">
        <div>
          <h2 class="text-2xl font-bold tracking-tight">Providers</h2>
          <p class="text-muted-foreground mt-1 text-sm">管理上游 LLM 供应商配置</p>
        </div>
        <div class="flex items-center gap-2">
          <button
            @click="refreshAll()"
            :disabled="store.providersLoading"
            class="hover:bg-accent inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
          >
            <RefreshCw :class="store.providersLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </button>
          <button
            @click="showAdd = !showAdd"
            class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
          >
            <Plus class="h-3.5 w-3.5" />
            Add Provider
          </button>
        </div>
      </div>

      <ProviderAddForm v-if="showAdd" @close="showAdd = false" @created="onCreated" />

      <!-- ── Filters ─────────────────────────────────────────────── -->
      <div class="grid gap-3 md:grid-cols-[1fr_200px_160px] md:items-end">
        <div>
          <label class="text-muted-foreground mb-1.5 block text-xs font-medium">Name / Model</label>
          <div class="relative">
            <Search
              class="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 h-4 w-4 -translate-y-1/2"
            />
            <input
              v-model="searchInput"
              type="text"
              placeholder="Search providers..."
              class="border-input bg-background ring-offset-background placeholder:text-muted-foreground focus-visible:ring-ring flex h-9 w-full rounded-md border px-3 py-2 pl-8 text-sm file:border-0 file:bg-transparent file:text-sm file:font-medium focus-visible:ring-2 focus-visible:ring-offset-2 focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-50"
            />
          </div>
        </div>
        <div>
          <label class="text-muted-foreground mb-1.5 block text-xs font-medium">Type</label>
          <Select
            :model-value="filterProviderType"
            @update:model-value="(value) => (filterProviderType = value as string)"
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">All types</SelectItem>
              <SelectItem value="openai">OpenAI</SelectItem>
              <SelectItem value="anthropic">Anthropic</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div>
          <label class="text-muted-foreground mb-1.5 block text-xs font-medium">Sort</label>
          <Select
            :model-value="sortByName ? 'name' : 'default'"
            @update:model-value="(value) => (sortByName = value === 'name')"
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="name">Name (A-Z)</SelectItem>
              <SelectItem value="default">Default</SelectItem>
            </SelectContent>
          </Select>
        </div>
      </div>
    </div>

    <!-- ── Loading ───────────────────────────────────────────────── -->
    <div
      v-if="store.providersLoading && store.providers.length === 0"
      class="text-muted-foreground py-12 text-center"
    >
      <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
      Loading…
    </div>

    <!-- ── Empty ─────────────────────────────────────────────────── -->
    <div
      v-else-if="filteredProviders.length === 0"
      class="text-muted-foreground py-12 text-center text-sm"
    >
      {{
        searchInput || filterProviderType !== "all"
          ? "No providers match the current filters."
          : 'No providers configured. Click "Add Provider" to get started.'
      }}
    </div>

    <!-- ── Table ─────────────────────────────────────────────────── -->
    <div v-else class="min-h-0 flex-1 rounded-lg border [&>div]:h-full">
      <TooltipProvider :delay-duration="200">
        <Table>
          <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
            <TableRow v-for="headerGroup in table.getHeaderGroups()" :key="headerGroup.id">
              <TableHead
                v-for="header in headerGroup.headers"
                :key="header.id"
                :class="(header.column.columnDef.meta as any)?.class"
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
                :class="(cell.column.columnDef.meta as any)?.class"
              >
                <FlexRender :render="cell.column.columnDef.cell!" :props="cell.getContext()" />
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </TooltipProvider>
    </div>

    <!-- ── Delete confirmation ───────────────────────────────────── -->
    <div
      v-if="confirmingDelete"
      class="bg-destructive/5 border-destructive/20 fixed inset-x-0 bottom-0 z-50 border-t p-4 shadow-lg md:relative md:inset-auto md:rounded-lg md:border md:shadow-none"
    >
      <div class="flex items-center justify-between gap-4">
        <p class="text-sm">
          Delete provider
          <span class="font-semibold">{{ confirmingDelete }}</span
          >?
        </p>
        <div class="flex gap-2">
          <Button variant="outline" size="sm" @click="confirmingDelete = null"> Cancel </Button>
          <Button
            variant="destructive"
            size="sm"
            :disabled="deleting"
            @click="confirmDelete(confirmingDelete)"
          >
            <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
            {{ deleting ? "Deleting…" : "Delete" }}
          </Button>
        </div>
      </div>
    </div>

    <!-- ── Edit Drawer / Dialog ──────────────────────────────────── -->
    <ProviderEditForm
      v-if="editingProvider"
      :provider="editingProvider"
      @close="onEditClose"
      @updated="onUpdated"
    />
  </div>
</template>

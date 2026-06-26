<script setup lang="ts">
import { Loader2, Pen, Plus, RefreshCw, Trash2 } from "@lucide/vue"
import { FlexRender, createColumnHelper, getCoreRowModel, useVueTable } from "@tanstack/vue-table"
import { h, onMounted, ref, watch } from "vue"
import { toast } from "vue-sonner"
import Combobox, { type ComboboxOption } from "@/components/Combobox.vue"
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
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import type { Provider } from "@/types/api"

const store = useProviderStore()

// ── Filters ───────────────────────────────────────────────────────
const filterModelName = ref("")
const filterProviderType = ref("all")
const sortByName = ref(true)

// Model combobox options — populated from all provider enabled_models
const modelOptions = ref<ComboboxOption[]>([])
const modelsLoading = ref(false)

// Add form
const showAdd = ref(false)

// Edit
const editingProvider = ref<Provider | null>(null)

// Delete
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)

// ── Data fetching ─────────────────────────────────────────────────
async function loadModelOptions() {
  modelsLoading.value = true
  try {
    await store.fetchAllProviderModels()
    modelOptions.value = store.allProviderModels.map((m) => ({ value: m, label: m }))
  } finally {
    modelsLoading.value = false
  }
}

async function fetchProviders() {
  await store.fetchProviders({
    model_name: filterModelName.value || undefined,
    provider_type: filterProviderType.value === "all" ? undefined : filterProviderType.value,
    sort: sortByName.value ? "name" : undefined,
  })
}

watch([filterModelName, filterProviderType, sortByName], () => {
  fetchProviders()
})

onMounted(async () => {
  await loadModelOptions()
  await fetchProviders()
})

// ── Enable / Disable toggle ───────────────────────────────────────
async function toggleEnabled(provider: Provider) {
  const newState = !provider.enabled
  await store.updateProvider(provider.name, { enabled: newState })
  toast.success(`Provider "${provider.name}" ${newState ? "enabled" : "disabled"}`)
  await fetchProviders()
}

// ── Add flow ──────────────────────────────────────────────────────
async function onCreated() {
  showAdd.value = false
  await loadModelOptions()
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
  await loadModelOptions()
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
    await loadModelOptions()
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
      h("span", { class: "font-mono text-xs text-muted-foreground" }, info.getValue()),
    meta: { class: "min-w-[200px] xl:min-w-[280px]" },
  }),
  columnHelper.accessor("models_endpoint", {
    header: "Models Endpoint",
    cell: (info) =>
      h("span", { class: "font-mono text-xs text-muted-foreground" }, info.getValue()),
    meta: { class: "min-w-[200px] xl:min-w-[280px]" },
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
    return store.providers
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
            @click="fetchProviders()"
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
          <label class="text-muted-foreground mb-1.5 block text-xs font-medium">Model</label>
          <Combobox
            v-model="filterModelName"
            :options="modelOptions"
            :loading="modelsLoading"
            placeholder="All models"
            search-placeholder="Search models..."
            empty-text="No models found"
          />
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
      v-else-if="store.providers.length === 0"
      class="text-muted-foreground py-12 text-center text-sm"
    >
      {{
        filterModelName || filterProviderType !== "all"
          ? "No providers match the current filters."
          : 'No providers configured. Click "Add Provider" to get started.'
      }}
    </div>

    <!-- ── Table ─────────────────────────────────────────────────── -->
    <div v-else class="min-h-0 flex-1 overflow-hidden rounded-lg border">
      <Table>
        <TableHeader class="bg-background">
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
        <TableBody class="overflow-auto">
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

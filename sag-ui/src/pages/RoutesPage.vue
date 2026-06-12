<script setup lang="ts">
import { AlertTriangle, ArrowUpDown, GitBranch, Loader2, Pen, Plus, RefreshCw, Trash2 } from "@lucide/vue"
import { FlexRender, createColumnHelper, getCoreRowModel, getSortedRowModel, useVueTable } from "@tanstack/vue-table"
import { computed, h, onMounted, ref } from "vue"
import RouteAddForm from "@/components/routes/RouteAddForm.vue"
import RouteEditForm from "@/components/routes/RouteEditForm.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()

// ── Add form ──────────────────────────────────────────────────────
const showAdd = ref(false)

async function handleRouteCreated() {
  showAdd.value = false
  await routeStore.fetchRoutes()
}

// ── Edit flow ─────────────────────────────────────────────────────
const editingRoute = ref<RouteConfig | null>(null)

function startEdit(route: RouteConfig) {
  confirmingDelete.value = null
  editingRoute.value = route
}

function onEditClose() {
  editingRoute.value = null
}

async function onUpdated() {
  editingRoute.value = null
  await routeStore.fetchRoutes()
}

// ── Delete flow ───────────────────────────────────────────────────
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)

function startDelete(modelName: string) {
  confirmingDelete.value = confirmingDelete.value === modelName ? null : modelName
}

async function confirmDelete(modelName: string) {
  deleting.value = true
  try {
    await routeStore.deleteRoute(modelName)
    confirmingDelete.value = null
    await routeStore.fetchRoutes()
  } catch {
    confirmingDelete.value = null
  } finally {
    deleting.value = false
  }
}

// ── Provider enabled lookup ──────────────────────────────────────
const disabledProviders = computed(() => {
  const set = new Set<string>()
  for (const p of providerStore.providers) {
    if (!p.enabled) set.add(p.name)
  }
  return set
})

function disabledTargetCount(route: RouteConfig): number {
  return route.targets.filter((t) => disabledProviders.value.has(t.provider)).length
}

onMounted(async () => {
  await Promise.all([providerStore.fetchProviders(), routeStore.fetchRoutes()])
})

// ══════════════════════════════════════════════════════════════════
// TanStack Table
// ══════════════════════════════════════════════════════════════════

const columnHelper = createColumnHelper<RouteConfig>()

const columns = [
  columnHelper.accessor("model_name", {
    header: "Model Name",
    cell: (info) =>
      h("div", { class: "flex items-center gap-2" }, [
        h(GitBranch, { class: "text-muted-foreground h-3.5 w-3.5 shrink-0" }),
        h("span", { class: "font-mono text-sm font-medium" }, info.getValue()),
      ]),
  }),
  columnHelper.accessor("api_endpoint", {
    header: "Endpoint",
    cell: (info) =>
      h(Badge, { variant: "outline", class: "text-xs font-normal" }, () => info.getValue()),
  }),
  columnHelper.accessor("strategy", {
    header: "Strategy",
    cell: (info) => {
      const s = info.getValue()
      return h(
        "span",
        {
          class: `rounded-full border px-2 py-0.5 text-[10px] ${
            s === "round_robin"
              ? "border-blue-200 text-blue-600"
              : "border-green-200 text-green-600"
          }`,
        },
        s === "round_robin" ? "round-robin" : "fill-first",
      )
    },
    meta: { class: "text-center" },
  }),
  columnHelper.accessor("targets", {
    header: "Targets",
    sortingFn: (a, b) => a.original.targets.length - b.original.targets.length,
    cell: (info) => {
      const total = info.getValue().length
      const disabled = disabledTargetCount(info.row.original)
      if (disabled === 0) {
        return h("span", { class: "tabular-nums font-medium" }, String(total))
      }
      return h(
        "div",
        { class: "flex items-center justify-center gap-1.5" },
        [
          h(AlertTriangle, {
            class: "text-amber-500 h-3.5 w-3.5 shrink-0",
          }),
          h("span", { class: "tabular-nums font-medium text-muted-foreground" }, [
            String(total - disabled),
            h("span", { class: "text-muted-foreground/60" }, `/${total}`),
          ]),
        ],
      )
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
            title: "Edit route",
            "aria-label": "Edit route",
          },
          () => h(Pen, { class: "h-3.5 w-3.5" }),
        ),
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            class:
              confirmingDelete.value === info.row.original.model_name ? "text-destructive" : "",
            onClick: () => startDelete(info.row.original.model_name),
            title:
              confirmingDelete.value === info.row.original.model_name
                ? "Cancel delete"
                : "Delete route",
            "aria-label":
              confirmingDelete.value === info.row.original.model_name
                ? "Cancel delete"
                : "Delete route",
          },
          () => h(Trash2, { class: "h-3.5 w-3.5" }),
        ),
      ]),
  }),
]

const table = useVueTable({
  get data() {
    return routeStore.routes
  },
  columns,
  getCoreRowModel: getCoreRowModel(),
  getSortedRowModel: getSortedRowModel(),
})
</script>

<template>
  <div class="space-y-6">
    <!-- ── Header ────────────────────────────────────────────────── -->
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Routes</h2>
        <p class="text-muted-foreground mt-1 text-sm">管理模型路由规则与 API 端点映射</p>
      </div>
      <div class="flex items-center gap-2">
        <button
          @click="routeStore.fetchRoutes()"
          :disabled="routeStore.routesLoading"
          class="hover:bg-accent inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
        >
          <RefreshCw :class="routeStore.routesLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </button>
        <button
          @click="showAdd = !showAdd"
          class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
        >
          <Plus class="h-3.5 w-3.5" />
          Add Route
        </button>
      </div>
    </div>

    <RouteAddForm
      v-if="showAdd"
      :providers="providerStore.providers"
      :provider-models="routeStore.providerModels"
      :provider-models-loading="routeStore.providerModelsLoading"
      @close="showAdd = false"
      @created="handleRouteCreated"
    />

    <!-- ── Loading ───────────────────────────────────────────────── -->
    <div
      v-if="routeStore.routesLoading && routeStore.routes.length === 0"
      class="text-muted-foreground py-12 text-center"
    >
      <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
      Loading…
    </div>

    <!-- ── Empty ─────────────────────────────────────────────────── -->
    <div
      v-else-if="routeStore.routes.length === 0"
      class="text-muted-foreground py-12 text-center text-sm"
    >
      No routes configured. Click "Add Route" to get started.
    </div>

    <!-- ── Table ─────────────────────────────────────────────────── -->
    <div v-else class="overflow-hidden rounded-lg border">
      <table class="w-full">
        <thead>
          <tr
            v-for="headerGroup in table.getHeaderGroups()"
            :key="headerGroup.id"
            class="bg-muted/50 border-b"
          >
            <th
              v-for="header in headerGroup.headers"
              :key="header.id"
              class="text-muted-foreground px-4 py-3 text-left text-xs font-medium"
              :class="[
                (header.column.columnDef.meta as any)?.class,
                header.column.getCanSort()
                  ? 'cursor-pointer select-none hover:text-foreground transition-colors'
                  : '',
              ]"
              @click="header.column.getCanSort() && header.column.toggleSorting()"
            >
              <div class="flex items-center gap-1">
                <FlexRender
                  v-if="!header.isPlaceholder"
                  :render="header.column.columnDef.header!"
                  :props="header.getContext()"
                />
                <ArrowUpDown
                  v-if="header.column.getCanSort()"
                  class="h-3 w-3"
                  :class="
                    header.column.getIsSorted()
                      ? 'text-foreground'
                      : 'text-muted-foreground/40'
                  "
                />
              </div>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="row in table.getRowModel().rows"
            :key="row.id"
            class="hover:bg-muted/30 border-b transition-colors last:border-b-0"
            :class="{
              'bg-muted/20 text-muted-foreground': disabledTargetCount(row.original) > 0,
            }"
          >
            <td
              v-for="cell in row.getVisibleCells()"
              :key="cell.id"
              class="px-4 py-3 text-sm"
              :class="(cell.column.columnDef.meta as any)?.class"
            >
              <FlexRender :render="cell.column.columnDef.cell!" :props="cell.getContext()" />
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <!-- ── Delete confirmation ───────────────────────────────────── -->
    <div
      v-if="confirmingDelete"
      class="bg-destructive/5 border-destructive/20 fixed inset-x-0 bottom-0 z-50 border-t p-4 shadow-lg md:relative md:inset-auto md:rounded-lg md:border md:shadow-none"
    >
      <div class="flex items-center justify-between gap-4">
        <p class="text-sm">
          Delete route
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

    <!-- ── Edit Dialog ────────────────────────────────────────────── -->
    <Dialog
      :open="editingRoute !== null"
      @update:open="
        (v) => {
          if (!v) onEditClose()
        }
      "
    >
      <DialogContent class="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Edit Route</DialogTitle>
          <DialogDescription>
            Modify routing rule for
            <span class="font-mono font-medium">{{ editingRoute?.model_name }}</span>
          </DialogDescription>
        </DialogHeader>
        <RouteEditForm
          v-if="editingRoute"
          :route="editingRoute"
          @close="onEditClose"
          @updated="onUpdated"
        />
      </DialogContent>
    </Dialog>
  </div>
</template>

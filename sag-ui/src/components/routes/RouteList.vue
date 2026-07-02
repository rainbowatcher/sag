<script setup lang="ts">
import { AlertTriangle, ArrowUpDown, GitBranch, Loader2, Pen, Trash2 } from "@lucide/vue"
import {
  FlexRender,
  createColumnHelper,
  getCoreRowModel,
  getSortedRowModel,
  useVueTable,
} from "@tanstack/vue-table"
import { computed, h, ref } from "vue"
import { toast } from "vue-sonner"
import RouteEditForm from "@/components/routes/RouteEditForm.vue"
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
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()

// ── Edit flow ─────────────────────────────────────────────────────
const editingRoute = ref<RouteConfig | null>(null)

function startEdit(route: RouteConfig) {
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
const deleteDialogOpen = ref(false)
const deleteTarget = ref<string | null>(null)
const deleting = ref(false)

function startDelete(modelName: string) {
  deleteTarget.value = modelName
  deleteDialogOpen.value = true
}

function cancelDelete() {
  deleteTarget.value = null
  deleteDialogOpen.value = false
}

async function confirmDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await routeStore.deleteRoute(deleteTarget.value)
    toast.success(`Route "${deleteTarget.value}" deleted`)
    deleteDialogOpen.value = false
    deleteTarget.value = null
    await routeStore.fetchRoutes()
  } catch (e: any) {
    const msg = getErrorMessage(e)
    toast.error(msg)
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

// ══════════════════════════════════════════════════════════════════
// TanStack Table
// ══════════════════════════════════════════════════════════════════

const columnHelper = createColumnHelper<RouteConfig>()

const strategyStyles: Record<string, string> = {
  sticky_failover:
    "bg-violet-500/10 text-violet-700 border-violet-500/20 dark:bg-violet-400/15 dark:text-violet-300 dark:border-violet-400/25",
  fill_first:
    "bg-emerald-500/10 text-emerald-700 border-emerald-500/20 dark:bg-emerald-400/15 dark:text-emerald-300 dark:border-emerald-400/25",
  round_robin:
    "bg-sky-500/10 text-sky-700 border-sky-500/20 dark:bg-sky-400/15 dark:text-sky-300 dark:border-sky-400/25",
}

const strategyLabels: Record<string, string> = {
  sticky_failover: "sticky-failover",
  fill_first: "fill-first",
  round_robin: "round-robin",
}

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
        Badge,
        {
          variant: "outline",
          class: `rounded-full px-2 text-[10px] font-normal ${strategyStyles[s] || ""}`,
        },
        () => strategyLabels[s] || s,
      )
    },
    meta: { class: "text-center 2xl:min-w-[140px]" },
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
      return h("div", { class: "flex items-center justify-center gap-1.5" }, [
        h(AlertTriangle, {
          class: "text-amber-500 h-3.5 w-3.5 shrink-0",
        }),
        h("span", { class: "tabular-nums font-medium text-muted-foreground" }, [
          String(total - disabled),
          h("span", { class: "text-muted-foreground/60" }, `/${total}`),
        ]),
      ])
    },
    meta: { class: "text-center xl:min-w-[100px]" },
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
            onClick: () => startDelete(info.row.original.model_name),
            title: "Delete route",
            "aria-label": "Delete route",
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
  <!-- ── Table ─────────────────────────────────────────────────── -->
  <div v-if="routeStore.routes.length > 0" class="h-full rounded-lg border [&>div]:h-full">
    <Table>
      <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
        <TableRow v-for="headerGroup in table.getHeaderGroups()" :key="headerGroup.id">
          <TableHead
            v-for="header in headerGroup.headers"
            :key="header.id"
            :class="[
              (header.column.columnDef.meta as any)?.class,
              header.column.getCanSort()
                ? 'hover:text-foreground cursor-pointer transition-colors select-none'
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
                  header.column.getIsSorted() ? 'text-foreground' : 'text-muted-foreground/40'
                "
              />
            </div>
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        <TableRow
          v-for="row in table.getRowModel().rows"
          :key="row.id"
          :class="{
            'bg-muted/20 text-muted-foreground': disabledTargetCount(row.original) > 0,
            'bg-destructive/5': deleteDialogOpen && deleteTarget === row.original.model_name,
          }"
        >
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

  <!-- ── Delete confirmation Dialog ────────────────────────────── -->
  <Dialog
    :open="deleteDialogOpen"
    @update:open="
      (v) => {
        if (!v) cancelDelete()
      }
    "
  >
    <DialogContent class="sm:max-w-md">
      <DialogHeader>
        <DialogTitle>Delete Route</DialogTitle>
        <DialogDescription>
          Are you sure you want to delete route
          <span class="font-mono font-medium">{{ deleteTarget }}</span
          >? This action cannot be undone.
        </DialogDescription>
      </DialogHeader>
      <DialogFooter class="flex flex-row justify-end gap-2 sm:justify-end">
        <Button variant="outline" size="sm" @click="cancelDelete" :disabled="deleting">
          Cancel
        </Button>
        <Button variant="destructive" size="sm" :disabled="deleting" @click="confirmDelete">
          <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
          {{ deleting ? "Deleting…" : "Delete" }}
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>

  <!-- ── Edit Dialog ─────────────────────────────────────────────── -->
  <Dialog
    :open="editingRoute !== null"
    @update:open="
      (v) => {
        if (!v) onEditClose()
      }
    "
  >
    <DialogContent class="max-h-[90vh] overflow-y-auto sm:max-w-3xl">
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
</template>

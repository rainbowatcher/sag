<script setup lang="ts">
import { AlertTriangle, ArrowUpDown, GitBranch, Loader2, Pen, Trash2 } from "@lucide/vue"
import {
  FlexRender,
  createColumnHelper,
  getCoreRowModel,
  getSortedRowModel,
  useVueTable,
} from "@tanstack/vue-table"
import { computed, h, onMounted, ref } from "vue"
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
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip"
import { cn, formatDuration, getErrorMessage } from "@/lib/utils"
import { useActiveTargetStore } from "@/stores/activeTarget"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig, ActiveTargetEntry } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const activeTargetStore = useActiveTargetStore()

onMounted(() => activeTargetStore.fetchActiveTargets())

// ── Search / filter ───────────────────────────────────────────────
// Debounced query from RoutesPage; client-side filter by model name.
const props = defineProps<{ searchQuery?: string }>()

const filteredRoutes = computed(() => {
  const kw = (props.searchQuery ?? "").trim().toLowerCase()
  if (!kw) return routeStore.routes
  return routeStore.routes.filter((r) => r.model_name.toLowerCase().includes(kw))
})

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
  await Promise.all([routeStore.fetchRoutes(), activeTargetStore.fetchActiveTargets()])
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
    await Promise.all([routeStore.fetchRoutes(), activeTargetStore.fetchActiveTargets()])
  } catch (e: any) {
    const msg = getErrorMessage(e)
    toast.error(msg)
  } finally {
    deleting.value = false
  }
}

// ── Clear sticky target / reset fill-first order ─────────────────
const clearingModel = ref<string | null>(null)

async function clearActiveTarget(entry: ActiveTargetEntry): Promise<boolean> {
  const isFillFirst = entry.strategy === "fill_first"
  // Fill-first orders are cached under the route model name (may be "*");
  // sticky pins under the request-model key (empty for idle entries).
  const model = isFillFirst ? entry.model : entry.key
  clearingModel.value = model
  try {
    await activeTargetStore.clearActiveTarget(model)
    toast.success(
      isFillFirst
        ? `Reset fill-first order for "${entry.model}", will re-shuffle on next request`
        : `Cleared sticky pin for "${entry.model}", will revert to priority routing`,
    )
    return true
  } catch (e: any) {
    toast.error(getErrorMessage(e, "Failed to clear active target"))
    return false
  } finally {
    clearingModel.value = null
  }
}

// ── Clear confirm dialog (triggered by clicking the target) ──────
const confirmOpen = ref(false)
const confirmEntry = ref<ActiveTargetEntry | null>(null)

function openClearConfirm(entry: ActiveTargetEntry) {
  confirmEntry.value = entry
  confirmOpen.value = true
}

function cancelClearConfirm() {
  confirmOpen.value = false
  confirmEntry.value = null
}

async function confirmClear() {
  const entry = confirmEntry.value
  if (!entry) return
  const ok = await clearActiveTarget(entry)
  if (ok) cancelClearConfirm()
}

// ── Target validity diagnosis ────────────────────────────────────
interface TargetIssue {
  targetIndex: number
  provider: string
  real_model?: string | null
  reason: "missing_provider" | "disabled_provider" | "invalid_model"
  message: string
}

interface RouteTargetStatus {
  total: number
  validCount: number
  invalidCount: number
  issues: TargetIssue[]
}

function getRouteTargetStatus(route: RouteConfig): RouteTargetStatus {
  const issues: TargetIssue[] = []
  let validCount = 0

  for (let i = 0; i < route.targets.length; i++) {
    const t = route.targets[i]
    const p = providerStore.providers.find((prov) => prov.name === t.provider)
    if (!p) {
      issues.push({
        targetIndex: i + 1,
        provider: t.provider,
        real_model: t.real_model,
        reason: "missing_provider",
        message: `Target #${i + 1}: Provider "${t.provider}" not found`,
      })
    } else if (!p.enabled) {
      issues.push({
        targetIndex: i + 1,
        provider: t.provider,
        real_model: t.real_model,
        reason: "disabled_provider",
        message: `Target #${i + 1}: Provider "${t.provider}" is disabled`,
      })
    } else if (p.enabled_models.length > 0) {
      const modelToCheck = t.real_model || route.model_name
      if (modelToCheck !== "*" && !p.enabled_models.includes(modelToCheck)) {
        issues.push({
          targetIndex: i + 1,
          provider: t.provider,
          real_model: t.real_model,
          reason: "invalid_model",
          message: `Target #${i + 1}: Model "${modelToCheck}" is not enabled for provider "${t.provider}"`,
        })
      } else {
        validCount++
      }
    } else {
      validCount++
    }
  }

  return {
    total: route.targets.length,
    validCount,
    invalidCount: issues.length,
    issues,
  }
}

/**
 * Best active-target entry for a route: prefer a live `pinned` pin over an
 * `expired` one, then the most recently recorded (smallest `age_secs`).
 * Wildcard routes (`model_name: "*"`) can pin several targets at once
 * (one per request model), so a plain `find` would return an arbitrary
 * first match — this helper picks the most meaningful one.
 */
function activeTargetFor(model: string): ActiveTargetEntry | undefined {
  const candidates = activeTargetStore.targets.filter(
    (s) => s.model === model && s.status !== "idle",
  )
  if (candidates.length === 0) return undefined
  const pinned = candidates.filter((s) => s.status === "pinned")
  const pool = pinned.length > 0 ? pinned : candidates
  return [...pool].sort((a, b) => a.age_secs - b.age_secs)[0]
}

/**
 * Current fill-first target for a route: the target at position 0 of the
 * cached shuffled order — the one currently absorbing all fill-first
 * traffic. Only meaningful for `fill_first` strategy routes (their entries
 * never carry a sticky pin, so `activeTargetFor` is always undefined);
 * returns undefined when the route is not fill-first or no order is cached.
 */
function fillFirstTargetFor(model: string): ActiveTargetEntry | undefined {
  return activeTargetStore.targets.find(
    (s) => s.model === model && s.strategy === "fill_first" && s.fill_first_position === 0,
  )
}

/**
 * Aligned label/value row for the active-target tooltip: label on the left,
 * value right-aligned on the same baseline so columns stay aligned.
 */
function tooltipRow(label: string, value: string, mono = false) {
  return h("div", { class: "flex items-baseline justify-between gap-4" }, [
    h("span", { class: "text-background/60" }, label),
    h("span", { class: mono ? "font-mono" : "" }, value),
  ])
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
    meta: { class: "2xl:min-w-[140px]" },
  }),
  columnHelper.accessor("targets", {
    header: "Targets",
    sortingFn: (a, b) => {
      const sa = getRouteTargetStatus(a.original).validCount
      const sb = getRouteTargetStatus(b.original).validCount
      return sa - sb
    },
    cell: (info) => {
      const status = getRouteTargetStatus(info.row.original)
      if (status.invalidCount === 0) {
        return h("span", { class: "tabular-nums font-medium" }, String(status.total))
      }
      const allInvalid = status.validCount === 0
      return h(Tooltip, null, () => [
        h(TooltipTrigger, { asChild: true }, () =>
          h("div", { class: "inline-flex items-center gap-1.5 cursor-help" }, [
            h(AlertTriangle, {
              class: cn("h-3.5 w-3.5 shrink-0", allInvalid ? "text-destructive" : "text-amber-500"),
            }),
            h(
              "span",
              {
                class: cn(
                  "tabular-nums font-medium",
                  allInvalid ? "text-destructive" : "text-muted-foreground",
                ),
              },
              [
                String(status.validCount),
                h("span", { class: "text-muted-foreground/60" }, `/${status.total}`),
              ],
            ),
          ]),
        ),
        h(TooltipContent, { side: "top", class: "max-w-xs space-y-1 text-xs" }, () => [
          h(
            "div",
            { class: "border-border/50 border-b pb-0.5 font-semibold" },
            allInvalid
              ? "All targets are unavailable"
              : `${status.validCount}/${status.total} targets valid`,
          ),
          ...status.issues.map((issue) =>
            h("div", { class: "text-muted-foreground" }, issue.message),
          ),
        ]),
      ])
    },
    meta: { class: "xl:min-w-[100px]" },
  }),
  columnHelper.display({
    id: "activeTarget",
    header: () => h("span", { class: "text-xs font-medium" }, "Active Target"),
    sortingFn: (a, b) => {
      const sa = activeTargetFor(a.original.model_name) ? 1 : 0
      const sb = activeTargetFor(b.original.model_name) ? 1 : 0
      return sb - sa
    },
    cell: (info) => {
      const model = info.row.original.model_name
      const sticky = activeTargetFor(model)
      const fill = fillFirstTargetFor(model)
      const active = sticky ?? fill

      if (!active) {
        return h("span", { class: "text-muted-foreground/50" }, "-")
      }

      const isFillFirst = fill !== undefined && sticky === undefined
      const dotClass = isFillFirst
        ? "bg-emerald-500"
        : active.status === "expired"
          ? "bg-amber-500"
          : "bg-green-500"

      // Status dot + provider label merged into one clickable button;
      // full details live in the aligned tooltip.
      return h(Tooltip, null, () => [
        h(TooltipTrigger, { asChild: true }, () =>
          h(
            Button,
            {
              variant: "ghost",
              size: "sm",
              class: "cursor-pointer gap-1.5 px-1 font-mono text-xs truncate max-w-[110px]",
              onClick: () => openClearConfirm(active),
            },
            () => [
              h("span", { class: cn("h-2 w-2 shrink-0 rounded-full", dotClass) }),
              h("span", { class: "truncate" }, active.provider),
            ],
          ),
        ),
        h(TooltipContent, { side: "left", class: "max-w-xs" }, () =>
          h("div", { class: "space-y-1" }, [
            tooltipRow("Provider", active.provider, true),
            tooltipRow("Model", active.real_model, true),
            tooltipRow("Status", isFillFirst ? "Fill-first target" : active.status),
            tooltipRow(
              isFillFirst ? "Order age" : "Pin age",
              formatDuration(
                (isFillFirst ? (active.fill_first_cache_age_secs ?? 0) : active.age_secs) * 1000,
              ),
              true,
            ),
            tooltipRow("Action", isFillFirst ? "Click to reset" : "Click to clear"),
          ]),
        ),
      ])
    },
    meta: { class: "xl:min-w-[220px]" },
  }),
  columnHelper.display({
    id: "actions",
    header: () => h("span", { class: "text-xs font-medium" }, "Actions"),
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
    return filteredRoutes.value
  },
  columns,
  getCoreRowModel: getCoreRowModel(),
  getSortedRowModel: getSortedRowModel(),
})
</script>

<template>
  <div v-if="routeStore.routes.length > 0" class="flex h-full flex-col gap-4">
    <!-- ── Empty (no match) ─────────────────────────────────────── -->
    <div v-if="filteredRoutes.length === 0" class="text-muted-foreground py-12 text-center text-sm">
      No routes match the current filters.
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
                  'bg-destructive/10 text-muted-foreground':
                    getRouteTargetStatus(row.original).validCount === 0,
                  'bg-muted/20 text-muted-foreground':
                    getRouteTargetStatus(row.original).validCount > 0 &&
                    getRouteTargetStatus(row.original).invalidCount > 0,
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
        </TooltipProvider>
      </div>
    </div>
  </div>

  <!-- ── Clear active target / reset fill-first confirm Dialog ──── -->
  <Dialog
    :open="confirmOpen"
    @update:open="
      (v) => {
        if (!v) cancelClearConfirm()
      }
    "
  >
    <DialogContent class="sm:max-w-md">
      <DialogHeader>
        <DialogTitle>
          {{
            confirmEntry?.strategy === "fill_first"
              ? "Reset Fill-First Order"
              : "Clear Active Target"
          }}
        </DialogTitle>
        <DialogDescription>
          <template v-if="confirmEntry?.strategy === 'fill_first'">
            Reset the cached fill-first order for
            <span class="font-mono font-medium">{{ confirmEntry?.model }}</span
            >? The next request will re-shuffle and may route to a different target.
          </template>
          <template v-else>
            Clear the active target
            <span class="font-mono font-medium"
              >{{ confirmEntry?.provider }}/{{ confirmEntry?.real_model }}</span
            >? Traffic will revert to priority routing.
          </template>
        </DialogDescription>
      </DialogHeader>
      <DialogFooter class="flex flex-row justify-end gap-2 sm:justify-end">
        <Button
          variant="outline"
          size="sm"
          :disabled="clearingModel !== null"
          @click="cancelClearConfirm"
        >
          Cancel
        </Button>
        <Button
          variant="destructive"
          size="sm"
          :disabled="clearingModel !== null"
          @click="confirmClear"
        >
          <Loader2 v-if="clearingModel !== null" class="h-3 w-3 animate-spin" />
          {{
            clearingModel !== null
              ? "Clearing…"
              : confirmEntry?.strategy === "fill_first"
                ? "Reset"
                : "Clear"
          }}
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>

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

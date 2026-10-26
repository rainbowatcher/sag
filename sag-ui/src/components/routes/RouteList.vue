<script setup lang="ts">
import { AlertTriangle, ArrowUpDown, GitBranch, Info, Loader2, Pen, Trash2 } from "@lucide/vue"
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
import SessionAffinityPopover from "@/components/routes/SessionAffinityPopover.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
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
import {
  activeTargetDotColor,
  clearActiveTargetModel,
  selectActiveTarget,
  selectFillFirstTarget,
} from "@/lib/activeTarget"
import { t } from "@/lib/i18n"
import { cn, formatDuration, getErrorMessage } from "@/lib/utils"
import { useActiveTargetStore } from "@/stores/activeTarget"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig, ActiveTargetEntry } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const activeTargetStore = useActiveTargetStore()

// Every modal in this list (edit / delete / clear-pin) is a Dialog on desktop
// and a bottom Drawer on mobile.
const { Modal } = useResponsiveModal()

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

function onEditOpenChange(v: boolean) {
  if (!v) onEditClose()
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

function onDeleteOpenChange(v: boolean) {
  if (!v) cancelDelete()
}

async function confirmDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await routeStore.deleteRoute(deleteTarget.value)
    toast.success(t("routes.toast.deleted", { name: deleteTarget.value }))
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

async function clearActiveTarget(entry: ActiveTargetEntry, isFillFirst: boolean): Promise<boolean> {
  // Fill-first order caches live under the route model name; sticky pins
  // under the request-model key. Which one to clear mirrors what the cell
  // displayed (`isFillFirst` from the display context) — not
  // `entry.strategy`, which reflects the route's *current* config and would
  // mislabel a leftover pin on a route that switched to fill_first.
  const model = clearActiveTargetModel(entry, isFillFirst)
  clearingModel.value = model
  try {
    await activeTargetStore.clearActiveTarget(model)
    toast.success(
      isFillFirst
        ? t("routes.toast.fillFirstReset", { model: entry.model })
        : t("routes.toast.pinCleared", { model: entry.model }),
    )
    return true
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("routes.toast.clearActiveFailed")))
    return false
  } finally {
    clearingModel.value = null
  }
}

// ── Clear confirm dialog (triggered by clicking the target) ──────
const confirmOpen = ref(false)
const confirmEntry = ref<ActiveTargetEntry | null>(null)
const confirmFillFirst = ref(false)

function openClearConfirm(entry: ActiveTargetEntry, isFillFirst: boolean) {
  confirmEntry.value = entry
  confirmFillFirst.value = isFillFirst
  confirmOpen.value = true
}

function cancelClearConfirm() {
  confirmOpen.value = false
  confirmEntry.value = null
}

function onConfirmOpenChange(v: boolean) {
  if (!v) cancelClearConfirm()
}

async function confirmClear() {
  const entry = confirmEntry.value
  if (!entry) return
  const ok = await clearActiveTarget(entry, confirmFillFirst.value)
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
    const target = route.targets[i]
    const p = providerStore.providers.find((prov) => prov.name === target.provider)
    if (!p) {
      issues.push({
        targetIndex: i + 1,
        provider: target.provider,
        real_model: target.real_model,
        reason: "missing_provider",
        message: t("routeList.targetMissingProvider", { index: i + 1, provider: target.provider }),
      })
    } else if (!p.enabled) {
      issues.push({
        targetIndex: i + 1,
        provider: target.provider,
        real_model: target.real_model,
        reason: "disabled_provider",
        message: t("routeList.targetProviderDisabled", { index: i + 1, provider: target.provider }),
      })
    } else if (p.enabled_models.length > 0) {
      const modelToCheck = target.real_model || route.model_name
      if (modelToCheck !== "*" && !p.enabled_models.includes(modelToCheck)) {
        issues.push({
          targetIndex: i + 1,
          provider: target.provider,
          real_model: target.real_model,
          reason: "invalid_model",
          message: t("routeList.targetModelDisabled", {
            index: i + 1,
            model: modelToCheck,
            provider: target.provider,
          }),
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
  session_sticky:
    "bg-fuchsia-500/10 text-fuchsia-700 border-fuchsia-500/20 dark:bg-fuchsia-400/15 dark:text-fuchsia-300 dark:border-fuchsia-400/25",
}

const strategyLabels = computed<Record<string, string>>(() => ({
  sticky_failover: t("routeList.strategy.stickyFailover"),
  fill_first: t("routeList.strategy.fillFirst"),
  round_robin: t("routeList.strategy.roundRobin"),
  session_sticky: t("routeList.strategy.sessionSticky"),
}))

// headers must stay functions: () => t(...) — string headers freeze on locale switch (see review-p1-p2 Minor-2)
const columns = [
  columnHelper.accessor("model_name", {
    header: () => t("routeList.columns.modelName"),
    cell: (info) =>
      h("div", { class: "flex items-center gap-2" }, [
        h(GitBranch, { class: "text-muted-foreground h-3.5 w-3.5 shrink-0" }),
        h("span", { class: "font-mono text-sm font-medium" }, info.getValue()),
      ]),
  }),
  columnHelper.display({
    id: "pipeline",
    header: () => t("routeList.columns.pipeline"),
    cell: (info) => {
      const r = info.row.original
      const isImage = (r.pipeline ?? "chat") === "image"
      const label = isImage && r.api_endpoint ? `image · ${r.api_endpoint}` : (r.pipeline ?? "chat")
      return h(
        Badge,
        {
          variant: "outline",
          class: "text-xs font-normal",
          title: r.api_endpoint ?? undefined,
        },
        () => label,
      )
    },
  }),
  columnHelper.accessor("strategy", {
    header: () => t("routeList.columns.strategy"),
    cell: (info) => {
      const s = info.getValue()
      return h(
        Badge,
        {
          variant: "outline",
          class: `rounded-full px-2 text-[10px] font-normal ${strategyStyles[s] || ""}`,
        },
        () => strategyLabels.value[s] || s,
      )
    },
    meta: { class: "2xl:min-w-[140px]" },
  }),
  columnHelper.accessor("targets", {
    header: () => t("routeList.columns.targets"),
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
              ? t("routeList.allTargetsUnavailable")
              : t("routeList.targetsValid", { valid: status.validCount, total: status.total }),
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
    header: () => h("span", { class: "text-xs font-medium" }, t("routeList.columns.activeTarget")),
    sortingFn: (a, b) => {
      // "Has an active target" sort, mirroring the cell: a sticky pin on a
      // sticky_failover route OR the current fill-first target (position 0
      // of the cached shuffled order). `selectFillFirstTarget` already
      // checks both strategy and position, so a fill_first route with no
      // cached order (every `fill_first_position` is null) is never
      // misjudged as active.
      const hasActive = (route: RouteConfig) => {
        const entries = activeTargetStore.targets
        return (
          selectActiveTarget(entries, route.model_name) !== undefined ||
          selectFillFirstTarget(entries, route.model_name) !== undefined
        )
      }
      return Number(hasActive(b.original)) - Number(hasActive(a.original))
    },
    cell: (info) => {
      const route = info.row.original
      const model = route.model_name
      const entries = activeTargetStore.targets
      const sticky = selectActiveTarget(entries, model)
      const fill = selectFillFirstTarget(entries, model)
      const active = sticky ?? fill

      // Only `session_sticky` routes keep per-session pins, so only they get
      // the popover; clearing a pin also refreshes the active-target column.
      const affinityPins =
        route.strategy === "session_sticky"
          ? h(SessionAffinityPopover, {
              model,
              onChanged: () => void activeTargetStore.fetchActiveTargets(),
            })
          : null

      // A route without an active target shows no target text. `session_sticky`
      // routes render only their sticky-pin popover, while other strategies
      // keep the "-" placeholder so the column does not go blank.
      if (!active) {
        if (route.strategy === "session_sticky") return affinityPins
        return h("span", { class: "text-muted-foreground/50" }, "-")
      }

      const isFillFirst = fill !== undefined && sticky === undefined
      const dotClass = activeTargetDotColor(sticky, fill)

      // Status dot + provider label merged into one clickable button;
      // full details live in the aligned tooltip.
      return h("div", { class: "flex items-center gap-1.5" }, [
        h(Tooltip, null, () => [
          h(TooltipTrigger, { asChild: true }, () =>
            h(
              Button,
              {
                variant: "ghost",
                size: "sm",
                class: "cursor-pointer gap-1.5 px-1 font-mono text-xs truncate max-w-[110px]",
                onClick: () => openClearConfirm(active, isFillFirst),
              },
              () => [
                h("span", { class: cn("h-2 w-2 shrink-0 rounded-full", dotClass) }),
                h("span", { class: "truncate" }, active.provider),
              ],
            ),
          ),
          h(TooltipContent, { side: "left", class: "max-w-xs" }, () =>
            h("div", { class: "space-y-1" }, [
              tooltipRow(t("common.provider"), active.provider, true),
              tooltipRow(t("common.model"), active.real_model, true),
              tooltipRow(
                t("common.status"),
                isFillFirst ? t("routeList.fillFirstTarget") : active.status,
              ),
              tooltipRow(
                isFillFirst ? t("routeList.tooltip.orderAge") : t("routeList.tooltip.pinAge"),
                formatDuration(
                  (isFillFirst ? (active.fill_first_cache_age_secs ?? 0) : active.age_secs) * 1000,
                ),
                true,
              ),
              tooltipRow(
                t("routeList.tooltip.action"),
                isFillFirst ? t("routeList.clickToReset") : t("routeList.clickToClear"),
              ),
            ]),
          ),
        ]),
        affinityPins,
      ])
    },
    meta: { class: "xl:min-w-[220px]" },
  }),
  columnHelper.display({
    id: "actions",
    header: () => h("span", { class: "text-xs font-medium" }, t("common.actions")),
    cell: (info) =>
      h("div", { class: "flex items-center justify-end gap-1" }, [
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            onClick: () => startEdit(info.row.original),
            title: t("routeList.editRoute"),
            "aria-label": t("routeList.editRoute"),
          },
          () => h(Pen, { class: "h-3.5 w-3.5" }),
        ),
        h(
          Button,
          {
            variant: "ghost",
            size: "icon-sm",
            onClick: () => startDelete(info.row.original.model_name),
            title: t("routeList.deleteRoute"),
            "aria-label": t("routeList.deleteRoute"),
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
      {{ $t("routeList.noMatch") }}
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
                    header.column.columnDef.meta?.class,
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
  </div>

  <!-- ── Clear active target / reset fill-first confirm Dialog / Drawer ── -->
  <component :is="Modal.Root" :open="confirmOpen" @update:open="onConfirmOpenChange">
    <component :is="Modal.Content" class="sm:max-w-md">
      <component :is="Modal.Header">
        <component :is="Modal.Title" class="flex items-center gap-1.5">
          {{
            confirmFillFirst
              ? $t("routeList.resetFillFirstTitle")
              : $t("routeList.clearActiveTitle")
          }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              <template v-if="confirmFillFirst">
                {{ $t("routeList.resetFillFirstDescLead")
                }}<span class="font-mono font-medium">{{ confirmEntry?.model }}</span
                >{{ $t("routeList.resetFillFirstDescTail") }}
              </template>
              <template v-else>
                {{ $t("routeList.clearActiveDescLead")
                }}<span class="font-mono font-medium"
                  >{{ confirmEntry?.provider }}/{{ confirmEntry?.real_model }}</span
                >{{ $t("routeList.clearActiveDescTail") }}
              </template>
            </TooltipContent>
          </Tooltip>
        </component>
        <component :is="Modal.Description" class="sr-only">
          <template v-if="confirmFillFirst">
            {{ $t("routeList.resetFillFirstDescLead") }}{{ confirmEntry?.model
            }}{{ $t("routeList.resetFillFirstDescTail") }}
          </template>
          <template v-else>
            {{ $t("routeList.clearActiveDescLead") }}{{ confirmEntry?.provider }}/{{
              confirmEntry?.real_model
            }}{{ $t("routeList.clearActiveDescTail") }}
          </template>
        </component>
      </component>
      <component :is="Modal.Footer">
        <Button
          variant="outline"
          size="sm"
          :disabled="clearingModel !== null"
          @click="cancelClearConfirm"
        >
          {{ $t("common.cancel") }}
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
              ? $t("routeList.clearing")
              : confirmFillFirst
                ? $t("routeList.reset")
                : $t("routeList.clear")
          }}
        </Button>
      </component>
    </component>
  </component>

  <!-- ── Delete confirmation Dialog / Drawer ─────────────────────── -->
  <component :is="Modal.Root" :open="deleteDialogOpen" @update:open="onDeleteOpenChange">
    <component :is="Modal.Content" class="sm:max-w-md">
      <component :is="Modal.Header">
        <component :is="Modal.Title" class="flex items-center gap-1.5">
          {{ $t("routeList.deleteTitle") }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              {{ $t("routeList.deleteDescLead")
              }}<span class="font-mono font-medium">{{ deleteTarget }}</span
              >{{ $t("routeList.deleteDescTail") }}
            </TooltipContent>
          </Tooltip>
        </component>
        <component :is="Modal.Description" class="sr-only">
          {{ $t("routeList.deleteDescLead") }}{{ deleteTarget }}{{ $t("routeList.deleteDescTail") }}
        </component>
      </component>
      <component :is="Modal.Footer">
        <Button variant="outline" size="sm" @click="cancelDelete" :disabled="deleting">
          {{ $t("common.cancel") }}
        </Button>
        <Button variant="destructive" size="sm" :disabled="deleting" @click="confirmDelete">
          <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
          {{ deleting ? $t("routeList.deleting") : $t("common.delete") }}
        </Button>
      </component>
    </component>
  </component>

  <!-- ── Edit Dialog / Drawer ─────────────────────────────────────── -->
  <component :is="Modal.Root" :open="editingRoute !== null" @update:open="onEditOpenChange">
    <component :is="Modal.Content" class="flex max-h-[90vh] flex-col overflow-hidden sm:max-w-3xl">
      <component :is="Modal.Header" class="shrink-0">
        <component :is="Modal.Title" class="flex items-center gap-1.5">
          {{ $t("routeList.editTitle") }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              {{ $t("routeList.editDescLead")
              }}<span class="font-mono font-medium">{{ editingRoute?.model_name }}</span>
            </TooltipContent>
          </Tooltip>
        </component>
        <component :is="Modal.Description" class="sr-only">
          {{ $t("routeList.editDescLead") }}{{ editingRoute?.model_name }}
        </component>
      </component>
      <div class="min-h-0 flex-1 overflow-y-auto px-4 pb-4 sm:px-0">
        <RouteEditForm
          v-if="editingRoute"
          :route="editingRoute"
          @close="onEditClose"
          @updated="onUpdated"
        />
      </div>
    </component>
  </component>
</template>

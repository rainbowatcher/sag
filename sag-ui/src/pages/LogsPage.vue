<script setup lang="ts">
import { AlertCircle, Loader2, ChevronLeft, ChevronRight, X } from "@lucide/vue"
import type { AcceptableValue } from "reka-ui"
import { ref, onMounted, computed, nextTick } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import SearchSelect from "@/components/SearchSelect.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Pagination,
  PaginationContent,
  PaginationEllipsis,
  PaginationItem,
  PaginationNext,
  PaginationPrevious,
} from "@/components/ui/pagination"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipTrigger, TooltipContent, TooltipProvider } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { formatDuration } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { CallLog } from "@/types/api"

const store = useAnalyticsStore()
const providerStore = useProviderStore()
const routeStore = useRouteStore()

// ── Filters ───────────────────────────────────────────────────────
const logsFilterProvider = ref("")
const logsFilterModel = ref("")
const logsFilterRealModel = ref("")
const logsFilterStatusCode = ref("")
// Hashed session identity — paste from the session-affinity snapshot.
const logsFilterSession = ref("")
// The session control shows its value as text unless the user is editing it.
const sessionEditing = ref(false)
const sessionInputRef = ref<HTMLInputElement>()

const statusCodeOptions = computed(() => [
  { value: "non_2xx", label: t("logs.status.non2xx") },
  { value: "4xx", label: t("logs.status.4xx") },
  { value: "5xx", label: t("logs.status.5xx") },
  { value: "200", label: t("logs.status.200") },
  { value: "400", label: t("logs.status.400") },
  { value: "401", label: t("logs.status.401") },
  { value: "403", label: t("logs.status.403") },
  { value: "404", label: t("logs.status.404") },
  { value: "422", label: t("logs.status.422") },
  { value: "429", label: t("logs.status.429") },
  { value: "500", label: t("logs.status.500") },
  { value: "502", label: t("logs.status.502") },
  { value: "503", label: t("logs.status.503") },
  { value: "504", label: t("logs.status.504") },
])

const providerOptions = computed(() =>
  providerStore.providers.map((p) => ({ value: p.name, label: p.name })),
)

const modelOptions = computed(() => providerStore.allProviderModels.map((m) => ({ value: m })))

const routeModelOptions = computed(() => routeStore.routes.map((r) => ({ value: r.model_name })))

async function refreshLogs() {
  const status_code = logsFilterStatusCode.value ? logsFilterStatusCode.value : undefined
  await store.applyLogsFilter({
    provider: logsFilterProvider.value || undefined,
    model: logsFilterModel.value || undefined,
    real_model: logsFilterRealModel.value || undefined,
    session: logsFilterSession.value || undefined,
    status_code,
  })
}

/**
 * Drop the applied session filter and hand the control back to its input state.
 * The remaining filters are re-applied (and the page reset) by the store.
 */
async function clearSessionFilter() {
  logsFilterSession.value = ""
  sessionEditing.value = true
  void nextTick(() => sessionInputRef.value?.focus())
  await store.applyLogsFilter({ ...store.activeLogFilter, session: undefined })
}

/** Swap the session control back to its input so the value can be edited. */
function editSessionFilter() {
  sessionEditing.value = true
  void nextTick(() => sessionInputRef.value?.focus())
}

/** Refresh the current page in place without resetting the offset. */
async function refreshCurrentPage() {
  await store.fetchLogs()
}

/** Number of leading session-key chars shown in the table (full key stays in `title`). */
const SESSION_KEY_SHOWN = 8

/**
 * Clicking a row's session cell applies that key to the session filter, reusing
 * the same state the session filter control operates on.
 */
async function applySessionFilter(sessionKey: string) {
  logsFilterSession.value = sessionKey
  await refreshLogs()
}

const logsCurrentPage = computed(() => Math.floor(store.logsOffset / store.logsLimit) + 1)
const showingStart = computed(() => (store.logsTotal === 0 ? 0 : store.logsOffset + 1))
const showingEnd = computed(() => Math.min(store.logsOffset + store.logs.length, store.logsTotal))

function onPageChange(page: number) {
  void store.setLogsPage(page)
}

function statusBadgeClass(code: number): string {
  if (code >= 500) {
    return "border-destructive/30 bg-destructive/10 text-destructive dark:text-red-400"
  }
  if (code >= 400) {
    return "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-300"
  }
  return "border-border text-foreground bg-transparent"
}

onMounted(() => {
  // Sync filter inputs from the persisted store state so the UI
  // reflects the active filter when returning to this page.
  logsFilterProvider.value = store.activeLogFilter.provider ?? ""
  logsFilterModel.value = store.activeLogFilter.model ?? ""
  logsFilterRealModel.value = store.activeLogFilter.real_model ?? ""
  logsFilterSession.value = store.activeLogFilter.session ?? ""
  logsFilterStatusCode.value =
    store.activeLogFilter.status_code != null ? String(store.activeLogFilter.status_code) : ""

  // Load provider and model data for SearchSelect options if not already loaded.
  if (providerStore.providers.length === 0) {
    void providerStore.fetchProviders()
  }
  if (providerStore.allProviderModels.length === 0) {
    void providerStore.fetchAllProviderModels()
  }
  if (routeStore.routes.length === 0) {
    void routeStore.fetchRoutes()
  }

  void store.fetchLogs()
})

function formatTs(iso: string): string {
  if (!iso) return ""
  try {
    return new Date(iso).toLocaleString("en-CA", { hour12: false }).replace(",", "")
  } catch {
    return iso
  }
}

/** Show only the product token of a User-Agent (e.g. `curl/8.0` → `curl`). */
function shortUserAgent(ua: string): string {
  return ua.split("/")[0].trim() || ua
}

/** Tokens/sec over the generation window after TTFT (post-TTFT streaming speed). */
function streamTpsFor(row: CallLog): number | null {
  const usage = row.usage
  const ttft = row.ttft_ms
  const upstream = row.upstream_duration_ms
  if (!usage || ttft == null || upstream == null) return null
  const genMs = upstream - ttft
  if (genMs <= 0) return null
  return (usage.completion_tokens * 1000) / genMs
}

/** Tokens/sec over the entire upstream duration (overall throughput including TTFT). */
function overallTpsFor(row: CallLog): number | null {
  const usage = row.usage
  const upstream = row.upstream_duration_ms
  if (!usage || !upstream) return null
  return (usage.completion_tokens * 1000) / upstream
}

function fmtTps(v: number | null): string | null {
  return v == null ? null : `${v.toFixed(1)} tps`
}

/** Per-request cache hit rate: cached_tokens / prompt_tokens, or null if not applicable. */
function cacheHitRate(row: CallLog): number | null {
  const usage = row.usage
  if (!usage || !usage.prompt_tokens || row.cached_tokens == null) return null
  return row.cached_tokens / usage.prompt_tokens
}

/** Map sentinel provider values to human-readable labels (reactive to locale). */
const sentinelLabels = computed<Record<string, string>>(() => ({
  __no_route__: t("logs.sentinel.noRoute"),
  __all_disabled__: t("logs.sentinel.allDisabled"),
  __all_exhausted__: t("logs.sentinel.allExhausted"),
}))

function formatProvider(raw: string): { label: string; isSentinel: boolean } {
  const label = sentinelLabels.value[raw]
  return label != null ? { label, isSentinel: true } : { label: raw, isSentinel: false }
}
</script>

<template>
  <div class="flex h-full min-w-0 flex-col">
    <div class="shrink-0 space-y-4">
      <div class="flex flex-nowrap items-center gap-2">
        <SearchSelect
          v-model="logsFilterProvider"
          :options="providerOptions"
          :placeholder="$t('logs.filter.provider')"
          :search-placeholder="$t('logs.filter.searchProvider')"
          class="flex min-h-[2rem] min-w-0 flex-1"
          @update:model-value="refreshLogs()"
        />
        <SearchSelect
          v-model="logsFilterModel"
          :options="routeModelOptions"
          :placeholder="$t('logs.filter.model')"
          :search-placeholder="$t('logs.filter.searchModel')"
          class="flex min-h-[2rem] min-w-0 flex-1"
          @update:model-value="refreshLogs()"
        />
        <SearchSelect
          v-model="logsFilterRealModel"
          :options="modelOptions"
          :placeholder="$t('logs.filter.realModel')"
          :search-placeholder="$t('logs.filter.searchRealModel')"
          class="hidden min-h-[2rem] xl:flex xl:min-w-0 xl:flex-1"
          @update:model-value="refreshLogs()"
        />
        <SearchSelect
          v-model="logsFilterStatusCode"
          :options="statusCodeOptions"
          :placeholder="$t('logs.filter.status')"
          :search-placeholder="$t('logs.filter.searchStatus')"
          class="hidden min-h-[2rem] xl:flex xl:min-w-0 xl:flex-1"
          @update:model-value="refreshLogs()"
        />
        <!-- Session filter: same trigger styling as SearchSelect, value inlined -->
        <div
          class="border-input bg-background focus-within:border-ring focus-within:ring-ring/50 flex min-h-[2rem] min-w-0 flex-1 flex-nowrap items-center gap-1 rounded-md border px-3 py-1 shadow-sm focus-within:ring-3 focus-within:outline-none"
        >
          <span
            v-if="logsFilterSession && !sessionEditing"
            :title="logsFilterSession"
            class="min-w-0 flex-1 cursor-text truncate font-mono text-sm"
            @click="editSessionFilter()"
          >
            {{ logsFilterSession }}
          </span>
          <input
            v-else
            ref="sessionInputRef"
            v-model="logsFilterSession"
            :placeholder="$t('logs.filter.session')"
            class="placeholder:text-muted-foreground min-w-[60px] flex-1 bg-transparent font-mono text-sm outline-none"
            @focus="sessionEditing = true"
            @blur="sessionEditing = false"
            @keyup.enter="refreshLogs()"
          />
          <button
            v-if="logsFilterSession"
            type="button"
            class="text-muted-foreground/60 hover:text-muted-foreground shrink-0 cursor-pointer"
            :aria-label="$t('logs.filter.sessionClear')"
            :title="$t('logs.filter.sessionClear')"
            @click.stop="clearSessionFilter()"
          >
            <X :size="14" />
          </button>
        </div>
        <RefreshButton
          :loading="store.logsLoading"
          class="hidden xl:ml-auto xl:inline-flex xl:shrink-0"
          @click="refreshCurrentPage()"
        />
      </div>
      <Alert v-if="store.logsError" variant="destructive">
        <AlertCircle class="h-4 w-4" />
        <AlertTitle>{{ $t("logs.errorTitle") }}</AlertTitle>
        <AlertDescription class="break-words">{{ store.logsError }}</AlertDescription>
      </Alert>
    </div>

    <div class="flex min-h-0 min-w-0 flex-1 flex-col pt-4">
      <div
        v-if="store.logsLoading && store.logs.length === 0"
        class="text-muted-foreground py-12 text-center"
      >
        <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
        {{ $t("common.loading") }}
      </div>
      <div
        v-else-if="store.logs.length === 0"
        class="text-muted-foreground py-12 text-center text-sm"
      >
        {{ $t("logs.empty") }}
      </div>
      <div v-else class="min-h-0 flex-1 overflow-auto rounded-lg border [&>div]:overflow-visible">
        <TooltipProvider :delay-duration="200">
          <Table>
            <TableHeader class="bg-background/95 sticky top-0 z-10 backdrop-blur-xs">
              <TableRow>
                <TableHead class="min-w-[130px] text-left xl:min-w-[155px]">{{
                  $t("logs.columns.time")
                }}</TableHead>
                <TableHead class="min-w-[90px] text-left xl:min-w-[110px]">{{
                  $t("common.provider")
                }}</TableHead>
                <TableHead class="min-w-[140px] text-left">{{
                  $t("logs.columns.client")
                }}</TableHead>
                <TableHead class="min-w-[80px] text-left">{{
                  $t("logs.columns.session")
                }}</TableHead>
                <TableHead class="min-w-[100px] text-left xl:min-w-[130px]">{{
                  $t("common.model")
                }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.prompt") }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.completion") }}</TableHead>
                <TableHead class="text-left">{{ $t("logs.columns.reasoning") }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.cached") }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.hit") }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.total") }}</TableHead>
                <TableHead class="text-right">{{ $t("logs.columns.duration") }}</TableHead>
                <TableHead class="text-right">{{ $t("common.status") }}</TableHead>
                <TableHead class="text-center">{{ $t("logs.columns.effect") }}</TableHead>
                <TableHead class="text-center">{{ $t("logs.columns.mode") }}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="row in store.logs" :key="row.request_id">
                <TableCell class="text-muted-foreground font-mono text-xs whitespace-nowrap">
                  {{ formatTs(row.timestamp) }}
                </TableCell>
                <TableCell
                  :class="[
                    formatProvider(row.provider).isSentinel
                      ? 'text-muted-foreground font-medium italic'
                      : 'font-medium',
                  ]"
                >
                  {{ formatProvider(row.provider).label }}
                </TableCell>
                <TableCell class="font-mono text-xs">
                  <span
                    v-if="row.user_agent"
                    :title="row.user_agent"
                    class="inline-block max-w-[140px] cursor-default truncate align-bottom"
                  >
                    {{ shortUserAgent(row.user_agent) }}
                  </span>
                  <span v-if="row.client_ip" class="text-muted-foreground text-[10px]">
                    {{ row.client_ip }}
                  </span>
                  <span v-if="!row.user_agent && !row.client_ip" class="text-muted-foreground">
                    —
                  </span>
                </TableCell>
                <TableCell class="font-mono text-xs">
                  <button
                    v-if="row.session_key"
                    type="button"
                    :title="row.session_key"
                    class="text-muted-foreground hover:text-foreground focus-visible:ring-ring cursor-pointer rounded-sm underline-offset-2 hover:underline focus-visible:ring-2 focus-visible:outline-none"
                    @click="applySessionFilter(row.session_key)"
                  >
                    {{ row.session_key.slice(0, SESSION_KEY_SHOWN) }}
                  </button>
                  <span v-else class="text-muted-foreground">—</span>
                </TableCell>
                <TableCell class="font-mono text-xs">
                  <div>{{ row.model }}</div>
                  <div
                    v-if="row.real_model && row.real_model !== row.model"
                    :class="[
                      'text-[10px]',
                      formatProvider(row.real_model).isSentinel
                        ? 'text-muted-foreground/60 italic'
                        : 'text-muted-foreground',
                    ]"
                  >
                    {{ formatProvider(row.real_model).label }}
                  </div>
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.usage?.prompt_tokens?.toLocaleString() ?? "—" }}
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.usage?.completion_tokens?.toLocaleString() ?? "—" }}
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.usage?.reasoning_tokens }}
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.cached_tokens?.toLocaleString() ?? "—" }}
                </TableCell>
                <TableCell class="text-right font-mono text-xs tabular-nums">
                  <span
                    v-if="cacheHitRate(row)"
                    :class="cacheHitRate(row)! > 0 ? 'text-green-600' : 'text-muted-foreground'"
                  >
                    {{ (cacheHitRate(row)! * 100).toFixed(1) + "%" }}
                  </span>
                  <span v-else class="text-muted-foreground">—</span>
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.usage?.total_tokens?.toLocaleString() ?? "—" }}
                </TableCell>
                <TableCell class="text-right font-mono text-xs tabular-nums">
                  <Tooltip>
                    <TooltipTrigger as-child>
                      <span class="cursor-default">{{ formatDuration(row.duration_ms) }}</span>
                    </TooltipTrigger>
                    <TooltipContent
                      v-if="row.upstream_duration_ms || row.ttft_ms || fmtTps(overallTpsFor(row))"
                      side="left"
                      class="font-mono text-xs tabular-nums"
                    >
                      <div class="grid grid-cols-[auto_auto] gap-x-4 gap-y-0.5">
                        <template v-if="row.upstream_duration_ms">
                          <span>{{ $t("logs.tooltip.upstream") }}</span>
                          <span class="text-right">{{
                            formatDuration(row.upstream_duration_ms)
                          }}</span>
                        </template>
                        <template v-if="row.ttft_ms">
                          <span>{{ $t("logs.tooltip.ttft") }}</span>
                          <span class="text-right">{{ formatDuration(row.ttft_ms) }}</span>
                        </template>
                        <template v-if="fmtTps(overallTpsFor(row))">
                          <span>{{ $t("logs.tooltip.tps") }}</span>
                          <span class="text-right text-green-600">{{
                            fmtTps(overallTpsFor(row))
                          }}</span>
                        </template>
                        <template v-if="fmtTps(streamTpsFor(row))">
                          <span>{{ $t("logs.tooltip.stream") }}</span>
                          <span class="text-right text-blue-600">{{
                            fmtTps(streamTpsFor(row))
                          }}</span>
                        </template>
                      </div>
                    </TooltipContent>
                  </Tooltip>
                </TableCell>
                <TableCell class="text-right">
                  <Tooltip>
                    <TooltipTrigger>
                      <Badge
                        variant="outline"
                        :class="statusBadgeClass(row.status_code)"
                        class="font-mono text-xs tabular-nums"
                      >
                        {{ row.status_code }}
                      </Badge>
                    </TooltipTrigger>
                    <TooltipContent v-if="row.error" class="max-w-md lg:max-w-lg">
                      {{ row.error }}
                    </TooltipContent>
                  </Tooltip>
                </TableCell>
                <TableCell>
                  <Badge variant="outline" class="text-xs">
                    {{ row.reasoning_effort ?? $t("logs.none") }}
                  </Badge>
                </TableCell>
                <TableCell class="text-center">
                  <Badge v-if="row.stream" variant="outline" class="text-sx">SSE</Badge>
                  <span v-else class="text-muted-foreground text-xs">—</span>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </TooltipProvider>
      </div>
    </div>

    <div class="shrink-0 pt-4">
      <div class="flex flex-wrap items-center justify-between gap-4 text-sm">
        <!-- Range info -->
        <div class="text-muted-foreground text-xs sm:text-sm">
          {{ $t("logs.showingLead")
          }}<span class="text-foreground font-medium tabular-nums">{{ showingStart }}</span
          >{{ $t("logs.showingTo")
          }}<span class="text-foreground font-medium tabular-nums">{{ showingEnd }}</span
          >{{ $t("logs.showingOf")
          }}<span class="text-foreground font-medium tabular-nums">{{ store.logsTotal }}</span
          >{{ $t("logs.showingTail") }}
        </div>

        <!-- comp-461 style Pagination aligned to the right -->
        <Pagination
          v-if="store.logsTotal > 0"
          :page="logsCurrentPage"
          :items-per-page="store.logsLimit"
          :total="store.logsTotal"
          :sibling-count="1"
          :show-edges="true"
          class="mx-0 w-auto"
          @update:page="onPageChange"
        >
          <PaginationContent
            v-slot="{ items }"
            class="flex items-center justify-end gap-0 -space-x-px"
          >
            <PaginationPrevious as-child>
              <Button
                variant="outline"
                class="size-8 rounded-none shadow-none first:rounded-s-md last:rounded-e-md focus-visible:z-10 aria-disabled:pointer-events-none [&[aria-disabled]>svg]:opacity-50"
                :disabled="logsCurrentPage <= 1 || store.logsLoading"
              >
                <ChevronLeft class="h-4 w-4" />
              </Button>
            </PaginationPrevious>

            <template v-for="(item, index) in items">
              <PaginationItem v-if="item.type === 'page'" :key="index" :value="item.value" as-child>
                <Button
                  variant="outline"
                  class="size-8 rounded-none shadow-none focus-visible:z-10 aria-disabled:pointer-events-none [&[aria-disabled]>svg]:opacity-50"
                  :class="item.value === logsCurrentPage && 'bg-accent font-semibold'"
                  :disabled="store.logsLoading"
                  @click="onPageChange(item.value)"
                >
                  {{ item.value }}
                </Button>
              </PaginationItem>
              <PaginationEllipsis
                v-else
                :key="item.type"
                :index="index"
                class="border-input flex size-8 items-center justify-center rounded-none border shadow-none"
              />
            </template>

            <PaginationNext as-child>
              <Button
                variant="outline"
                class="size-8 rounded-none shadow-none first:rounded-s-md last:rounded-e-md focus-visible:z-10 aria-disabled:pointer-events-none [&[aria-disabled]>svg]:opacity-50"
                :disabled="!store.hasMoreLogs || store.logsLoading"
              >
                <ChevronRight class="h-4 w-4" />
              </Button>
            </PaginationNext>
          </PaginationContent>
        </Pagination>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { AlertCircle, Search, RefreshCw, Loader2, ChevronLeft, ChevronRight } from "@lucide/vue"
import { ref, onMounted, computed } from "vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipTrigger, TooltipContent } from "@/components/ui/tooltip"
import { formatDuration } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"
import type { CallLog } from "@/types/api"

const store = useAnalyticsStore()

// ── Filters ───────────────────────────────────────────────────────
const logsFilterProvider = ref("")
const logsFilterModel = ref("")
const logsFilterRealModel = ref("")
const logsFilterStatusCode = ref("")

async function refreshLogs() {
  store.resetLogsPage()
  // The browser <input type="number"> only validates manual key entry —
  // pasted text like "abc" slips through and would yield `Number("abc") →
  // NaN`, which `String(NaN) → "NaN"` would then fail Option<u16>
  // deserialization in the backend with HTTP 400. Reject on the client
  // and silently drop the filter (treat as "no status filter") instead of
  // forwarding an invalid value.
  let status_code: number | undefined
  const trim = logsFilterStatusCode.value.trim()
  if (trim.length > 0) {
    const n = Number(trim)
    if (Number.isFinite(n) && Number.isInteger(n) && n >= 0 && n <= 65535) {
      status_code = n
    }
  }
  await store.fetchLogs({
    provider: logsFilterProvider.value || undefined,
    model: logsFilterModel.value || undefined,
    real_model: logsFilterRealModel.value || undefined,
    status_code,
  })
}

const logsCurrentPage = computed(() => Math.floor(store.logsOffset / store.logsLimit) + 1)

function statusVariant(code: number) {
  if (code >= 500) return "destructive" as const
  if (code >= 400) return "secondary" as const
  return "outline" as const
}

onMounted(() => {
  store.fetchLogs()
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

/** Tokens/sec over the generation window after TTFT; null when inputs are missing or invalid. */
function tpsFor(row: CallLog): number | null {
  const usage = row.usage
  const ttft = row.ttft_ms
  const upstream = row.upstream_duration_ms
  if (!usage || ttft == null || upstream == null) return null
  const genMs = upstream - ttft
  if (genMs <= 0) return null
  return (usage.completion_tokens * 1000) / genMs
}

function fmtTps(row: CallLog): string | null {
  const tps = tpsFor(row)
  return tps == null ? null : `${tps.toFixed(1)} tps`
}

/** Per-request cache hit rate: cached_tokens / prompt_tokens, or null if not applicable. */
function cacheHitRate(row: CallLog): number | null {
  const usage = row.usage
  if (!usage || !usage.prompt_tokens || row.cached_tokens == null) return null
  return row.cached_tokens / usage.prompt_tokens
}
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0 space-y-6">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Request Logs</h2>
        <p class="text-muted-foreground mt-1 text-sm">请求记录明细 / 调用日志</p>
      </div>

      <div class="flex flex-wrap items-center gap-3">
        <Input
          v-model="logsFilterProvider"
          class="w-40 xl:w-52"
          placeholder="provider"
          @keyup.enter="refreshLogs()"
        />
        <Input
          v-model="logsFilterModel"
          class="w-40 xl:w-52"
          placeholder="model"
          @keyup.enter="refreshLogs()"
        />
        <Input
          v-model="logsFilterRealModel"
          class="w-40 xl:w-52"
          placeholder="real_model"
          @keyup.enter="refreshLogs()"
        />
        <Input
          v-model="logsFilterStatusCode"
          type="number"
          min="0"
          class="w-28"
          placeholder="status"
          @keyup.enter="refreshLogs()"
        />
        <Button @click="refreshLogs()">
          <Search class="h-3.5 w-3.5" />
          Query
        </Button>
        <Button
          variant="outline"
          class="ml-auto"
          :disabled="store.logsLoading"
          @click="refreshLogs()"
        >
          <RefreshCw :class="store.logsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </Button>
      </div>

      <Alert v-if="store.logsError" variant="destructive">
        <AlertCircle class="h-4 w-4" />
        <AlertTitle>Failed to load call logs</AlertTitle>
        <AlertDescription class="break-words">{{ store.logsError }}</AlertDescription>
      </Alert>
    </div>

    <div class="min-h-0 flex-1 pt-6">
      <div class="h-full rounded-lg border [&>div]:h-full">
        <div
          v-if="store.logsLoading"
          class="text-muted-foreground flex h-full items-center justify-center gap-2"
        >
          <Loader2 class="h-5 w-5 animate-spin" /> Loading...
        </div>
        <div
          v-else-if="store.logs.length === 0"
          class="text-muted-foreground flex h-full items-center justify-center text-sm"
        >
          No call logs match the current filter.
        </div>
        <Table v-else>
          <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
            <TableRow>
              <TableHead class="min-w-[130px] text-left xl:min-w-[155px]">Time</TableHead>
              <TableHead class="min-w-[90px] text-left xl:min-w-[110px]">Provider</TableHead>
              <TableHead class="min-w-[140px] text-left">Client</TableHead>
              <TableHead class="min-w-[100px] text-left xl:min-w-[130px]">Model</TableHead>
              <TableHead class="text-right">Prompt</TableHead>
              <TableHead class="text-right">Completion</TableHead>
              <TableHead class="text-left">Reasoning</TableHead>
              <TableHead class="text-right">Cached</TableHead>
              <TableHead class="text-right">Hit</TableHead>
              <TableHead class="text-right">Total</TableHead>
              <TableHead class="text-right">Duration</TableHead>
              <TableHead class="text-right">Status</TableHead>
              <TableHead class="text-center">Effect</TableHead>
              <TableHead class="text-center">Mode</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow v-for="row in store.logs" :key="row.request_id">
              <TableCell class="text-muted-foreground font-mono text-xs whitespace-nowrap">
                {{ formatTs(row.timestamp) }}
              </TableCell>
              <TableCell class="font-medium">{{ row.provider }}</TableCell>
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
                <div>{{ row.model }}</div>
                <div
                  v-if="row.real_model && row.real_model !== row.model"
                  class="text-muted-foreground text-[10px]"
                >
                  {{ row.real_model }}
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
                    v-if="row.upstream_duration_ms || row.ttft_ms || fmtTps(row)"
                    side="left"
                    class="font-mono text-xs tabular-nums"
                  >
                    <div class="grid grid-cols-[auto_auto] gap-x-4 gap-y-0.5">
                      <template v-if="row.upstream_duration_ms">
                        <span>upstream</span>
                        <span class="text-right">{{
                          formatDuration(row.upstream_duration_ms)
                        }}</span>
                      </template>
                      <template v-if="row.ttft_ms">
                        <span>ttft</span>
                        <span class="text-right">{{ formatDuration(row.ttft_ms) }}</span>
                      </template>
                      <template v-if="fmtTps(row)">
                        <span>tps</span>
                        <span class="text-right text-green-600">{{ fmtTps(row) }}</span>
                      </template>
                    </div>
                  </TooltipContent>
                </Tooltip>
              </TableCell>
              <TableCell class="text-right">
                <Tooltip>
                  <TooltipTrigger>
                    <Badge
                      :variant="statusVariant(row.status_code)"
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
                  {{ row.reasoning_effort ?? "none" }}
                </Badge>
              </TableCell>
              <TableCell class="text-center">
                <Badge v-if="row.stream" variant="outline" class="text-sx">SSE</Badge>
                <span v-else class="text-muted-foreground text-xs">—</span>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </div>
    </div>

    <div class="shrink-0 pt-6">
      <div class="flex items-center justify-between gap-4 text-sm">
        <div class="text-muted-foreground">
          Page <span class="text-foreground font-medium tabular-nums">{{ logsCurrentPage }}</span>
          <span v-if="store.logs.length > 0">
            · showing {{ store.logs.length }} row{{ store.logs.length === 1 ? "" : "s" }}
          </span>
        </div>
        <div class="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            :disabled="store.logsOffset === 0 || store.logsLoading"
            @click="store.prevLogsPage()"
          >
            <ChevronLeft class="h-3.5 w-3.5" />
            Prev
          </Button>
          <Button
            variant="outline"
            size="sm"
            :disabled="!store.hasMoreLogs || store.logsLoading"
            @click="store.nextLogsPage()"
          >
            Next
            <ChevronRight class="h-3.5 w-3.5" />
          </Button>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { AlertCircle, Search, Loader2, TrendingUp, Zap, Clock } from "@lucide/vue"
import { ref, onMounted, computed } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs"
import { formatCompactNumber, formatDuration, successRateColor } from "@/lib/utils"
import { useAnalyticsStore } from "@/stores/analytics"

const store = useAnalyticsStore()

// ── Cache tab state ────────────────────────────────────────────────
const cacheFilterProvider = ref("")
const cacheFilterModel = ref("")

async function refreshCache() {
  await store.fetchCacheStats(
    cacheFilterProvider.value || undefined,
    cacheFilterModel.value || undefined,
  )
}

// ── Stats tab state ────────────────────────────────────────────────
const statsFilterProvider = ref("")
const statsFilterModel = ref("")

onMounted(() => {
  store.fetchStats()
})

async function refreshStats() {
  await store.fetchStats(
    statsFilterProvider.value || undefined,
    statsFilterModel.value || undefined,
  )
}

const statsTotals = computed(() => {
  let calls = 0
  let tokens = 0
  let errors = 0
  let weightedMs = 0
  for (const r of store.stats) {
    calls += r.call_count
    tokens += r.total_tokens
    errors += r.error_count
    weightedMs += r.avg_duration_ms * r.call_count
  }
  const successRate = calls > 0 ? ((calls - errors) / calls) * 100 : null
  return {
    calls,
    tokens,
    avgMs: calls > 0 ? weightedMs / calls : 0,
    successRate,
  }
})
</script>

<template>
  <div class="flex h-full flex-col">
    <Tabs :default-value="'stats'" class="flex min-h-0 flex-1 flex-col pt-4">
      <TabsList class="shrink-0">
        <TabsTrigger value="stats">Stats</TabsTrigger>
        <TabsTrigger value="cache">Cache</TabsTrigger>
      </TabsList>

      <!-- ── Stats panel ─────────────────────────────────────────── -->
      <TabsContent value="stats" class="min-h-0 flex-1">
        <div class="flex h-full flex-col">
          <div class="shrink-0 space-y-4">
            <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
              <Card>
                <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                  <TrendingUp class="h-5 w-5 text-blue-500" />
                  <div>
                    <p class="text-muted-foreground text-xs">Total Calls</p>
                    <p class="text-lg font-bold tabular-nums xl:text-xl">
                      {{ statsTotals.calls.toLocaleString() }}
                    </p>
                  </div>
                </CardContent>
              </Card>
              <Card>
                <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                  <Zap class="h-5 w-5 text-amber-500" />
                  <div>
                    <p class="text-muted-foreground text-xs">Total Tokens</p>
                    <p class="text-lg font-bold tabular-nums xl:text-xl">
                      {{ formatCompactNumber(statsTotals.tokens) }}
                    </p>
                  </div>
                </CardContent>
              </Card>
              <Card>
                <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                  <Clock class="h-5 w-5 text-green-500" />
                  <div>
                    <p class="text-muted-foreground text-xs">Avg Latency</p>
                    <p class="text-lg font-bold tabular-nums xl:text-xl">
                      {{ formatDuration(statsTotals.avgMs) }}
                    </p>
                  </div>
                </CardContent>
              </Card>
              <Card>
                <CardContent class="flex items-center gap-3 p-4 xl:gap-4">
                  <Badge
                    v-if="statsTotals.successRate !== null"
                    variant="outline"
                    class="text-[10px]"
                    :style="{
                      borderColor: successRateColor(statsTotals.successRate),
                      color: successRateColor(statsTotals.successRate),
                    }"
                  >
                    {{ statsTotals.successRate >= 95 ? "✓" : "!" }}
                  </Badge>
                  <Badge v-else variant="outline" class="text-muted-foreground text-[10px]">
                    —
                  </Badge>
                  <div>
                    <p class="text-muted-foreground text-xs">Success Rate</p>
                    <p
                      v-if="statsTotals.successRate !== null"
                      class="text-lg font-bold tabular-nums xl:text-xl"
                      :style="{ color: successRateColor(statsTotals.successRate) }"
                    >
                      {{ statsTotals.successRate.toFixed(1) }}%
                    </p>
                    <p v-else class="text-muted-foreground text-lg">—</p>
                  </div>
                </CardContent>
              </Card>
            </div>

            <div class="flex items-center gap-3">
              <Input
                v-model="statsFilterProvider"
                class="w-44 xl:w-56"
                placeholder="Filter provider..."
                @keyup.enter="refreshStats()"
              />
              <Input
                v-model="statsFilterModel"
                class="w-44 xl:w-56"
                placeholder="Filter model..."
                @keyup.enter="refreshStats()"
              />
              <Button @click="refreshStats()">
                <Search class="h-3.5 w-3.5" />
                Query
              </Button>
              <RefreshButton
                :loading="store.statsLoading"
                size="default"
                class="ml-auto"
                @click="refreshStats()"
              />
            </div>

            <Alert v-if="store.statsError" variant="destructive">
              <AlertCircle class="h-4 w-4" />
              <AlertTitle>Failed to load statistics</AlertTitle>
              <AlertDescription class="break-words">{{ store.statsError }}</AlertDescription>
              <Button variant="destructive" size="sm" class="mt-2" @click="refreshStats()">
                Retry
              </Button>
            </Alert>
          </div>

          <div class="min-h-0 flex-1">
            <div class="h-full rounded-lg border [&>div]:h-full">
              <div
                v-if="store.statsLoading"
                class="text-muted-foreground flex h-full items-center justify-center gap-2"
              >
                <Loader2 class="h-5 w-5 animate-spin" /> Loading...
              </div>
              <div
                v-else-if="store.stats.length === 0"
                class="text-muted-foreground flex h-full items-center justify-center text-sm"
              >
                No statistics available.
              </div>
              <Table v-else>
                <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
                  <TableRow>
                    <TableHead class="text-left">Provider</TableHead>
                    <TableHead class="text-left">Model</TableHead>
                    <TableHead class="text-left">Real Model</TableHead>
                    <TableHead class="text-right">Calls</TableHead>
                    <TableHead class="text-right">Tokens</TableHead>
                    <TableHead class="text-right">Avg Latency</TableHead>
                    <TableHead class="text-right">Success Rate</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  <TableRow v-for="(row, i) in store.stats" :key="i">
                    <TableCell class="font-medium">{{ row.provider }}</TableCell>
                    <TableCell class="font-mono text-xs">{{ row.model }}</TableCell>
                    <TableCell class="font-mono text-xs">{{ row.real_model }}</TableCell>
                    <TableCell class="text-right tabular-nums">
                      {{ row.call_count.toLocaleString() }}
                    </TableCell>
                    <TableCell class="text-right tabular-nums">
                      {{ formatCompactNumber(row.total_tokens) }}
                    </TableCell>
                    <TableCell class="text-right font-mono text-xs tabular-nums">
                      {{ formatDuration(row.avg_duration_ms) }}
                    </TableCell>
                    <TableCell class="text-right font-mono text-xs tabular-nums">
                      <template v-if="row.call_count > 0">
                        <span
                          :style="{
                            color: successRateColor(
                              ((row.call_count - row.error_count) / row.call_count) * 100,
                            ),
                          }"
                        >
                          {{
                            (((row.call_count - row.error_count) / row.call_count) * 100).toFixed(
                              1,
                            )
                          }}%
                        </span>
                      </template>
                      <span v-else class="text-muted-foreground">—</span>
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </div>
          </div>
        </div>
      </TabsContent>

      <!-- ── Cache panel ─────────────────────────────────────────── -->
      <TabsContent value="cache" class="min-h-0 flex-1">
        <div class="flex h-full flex-col">
          <div class="shrink-0 space-y-4">
            <div class="flex items-center gap-3">
              <Input
                v-model="cacheFilterProvider"
                class="w-44 xl:w-56"
                placeholder="Filter provider..."
                @keyup.enter="refreshCache()"
              />
              <Input
                v-model="cacheFilterModel"
                class="w-44 xl:w-56"
                placeholder="Filter model..."
                @keyup.enter="refreshCache()"
              />
              <Button @click="refreshCache()">
                <Search class="h-3.5 w-3.5" />
                Query
              </Button>
              <RefreshButton
                :loading="store.cacheLoading"
                size="default"
                class="ml-auto"
                @click="refreshCache()"
              />
            </div>

            <Alert v-if="store.cacheError" variant="destructive">
              <AlertCircle class="h-4 w-4" />
              <AlertTitle>Failed to load cache statistics</AlertTitle>
              <AlertDescription class="break-words">{{ store.cacheError }}</AlertDescription>
              <Button variant="destructive" size="sm" class="mt-2" @click="refreshCache()">
                Retry
              </Button>
            </Alert>
          </div>

          <div class="min-h-0 flex-1">
            <div class="h-full rounded-lg border [&>div]:h-full">
              <div
                v-if="store.cacheLoading"
                class="text-muted-foreground flex h-full items-center justify-center gap-2"
              >
                <Loader2 class="h-5 w-5 animate-spin" /> Loading...
              </div>
              <div
                v-else-if="store.cache.length === 0"
                class="text-muted-foreground flex h-full items-center justify-center text-sm"
              >
                No cache statistics available.
              </div>
              <Table v-else>
                <TableHeader class="bg-background/90 sticky top-0 z-10 backdrop-blur-xs">
                  <TableRow>
                    <TableHead class="text-left">Provider</TableHead>
                    <TableHead class="text-left">Model</TableHead>
                    <TableHead class="text-left">Real Model</TableHead>
                    <TableHead class="text-right">Calls</TableHead>
                    <TableHead class="text-right">Prompt Tokens</TableHead>
                    <TableHead class="text-right">Cached Tokens</TableHead>
                    <TableHead class="text-right">Hit Rate</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  <TableRow v-for="(row, i) in store.cache" :key="i">
                    <TableCell class="font-medium">{{ row.provider }}</TableCell>
                    <TableCell class="font-mono text-xs">{{ row.model }}</TableCell>
                    <TableCell class="font-mono text-xs">{{ row.real_model }}</TableCell>
                    <TableCell class="text-right tabular-nums">
                      {{ row.total_calls.toLocaleString() }}
                    </TableCell>
                    <TableCell class="text-right tabular-nums">
                      {{ formatCompactNumber(row.total_prompt_tokens) }}
                    </TableCell>
                    <TableCell class="text-right tabular-nums">
                      {{ formatCompactNumber(row.total_cached_tokens) }}
                    </TableCell>
                    <TableCell class="text-right font-mono text-xs tabular-nums">
                      <span
                        v-if="row.total_prompt_tokens > 0"
                        :class="
                          row.total_cached_tokens / row.total_prompt_tokens > 0
                            ? 'text-green-600'
                            : 'text-muted-foreground'
                        "
                      >
                        {{
                          ((row.total_cached_tokens / row.total_prompt_tokens) * 100).toFixed(1) +
                          "%"
                        }}
                      </span>
                      <span v-else class="text-muted-foreground">—</span>
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </div>
          </div>
        </div>
      </TabsContent>
    </Tabs>
  </div>
</template>

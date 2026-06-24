<script setup lang="ts">
import { AlertCircle, Search, RefreshCw, Loader2, TrendingUp, Zap, Clock } from "@lucide/vue"
import { ref, onMounted, computed } from "vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
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
import { formatDuration, successRateColor } from "@/lib/utils"
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
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Analytics</h2>
        <p class="text-muted-foreground mt-1 text-sm">调用统计 / Token 消耗 / 缓存命中</p>
      </div>
    </div>

    <Tabs :default-value="'stats'" class="space-y-6">
      <TabsList>
        <TabsTrigger value="stats">Stats</TabsTrigger>
        <TabsTrigger value="cache">Cache</TabsTrigger>
      </TabsList>

      <!-- ── Stats panel ─────────────────────────────────────────── -->
      <TabsContent value="stats" class="space-y-6">
        <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
          <Card>
            <CardContent class="flex items-center gap-3 p-4">
              <TrendingUp class="h-5 w-5 text-blue-500" />
              <div>
                <p class="text-muted-foreground text-xs">Total Calls</p>
                <p class="text-lg font-bold tabular-nums">
                  {{ statsTotals.calls.toLocaleString() }}
                </p>
              </div>
            </CardContent>
          </Card>
          <Card>
            <CardContent class="flex items-center gap-3 p-4">
              <Zap class="h-5 w-5 text-amber-500" />
              <div>
                <p class="text-muted-foreground text-xs">Total Tokens</p>
                <p class="text-lg font-bold tabular-nums">
                  {{ statsTotals.tokens.toLocaleString() }}
                </p>
              </div>
            </CardContent>
          </Card>
          <Card>
            <CardContent class="flex items-center gap-3 p-4">
              <Clock class="h-5 w-5 text-green-500" />
              <div>
                <p class="text-muted-foreground text-xs">Avg Latency</p>
                <p class="text-lg font-bold tabular-nums">
                  {{ formatDuration(statsTotals.avgMs) }}
                </p>
              </div>
            </CardContent>
          </Card>
          <Card>
            <CardContent class="flex items-center gap-3 p-4">
              <div
                v-if="statsTotals.successRate !== null"
                class="flex h-5 w-5 items-center justify-center rounded-full border-2 text-[10px] font-bold"
                :style="{
                  borderColor: successRateColor(statsTotals.successRate),
                  color: successRateColor(statsTotals.successRate),
                }"
              >
                {{ statsTotals.successRate >= 95 ? "✓" : "!" }}
              </div>
              <div
                v-else
                class="border-muted-foreground/30 text-muted-foreground flex h-5 w-5 items-center justify-center rounded-full border-2 text-[10px] font-bold"
              >
                —
              </div>
              <div>
                <p class="text-muted-foreground text-xs">Success Rate</p>
                <p
                  v-if="statsTotals.successRate !== null"
                  class="text-lg font-bold tabular-nums"
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
            class="w-44"
            placeholder="Filter provider..."
            @keyup.enter="refreshStats()"
          />
          <Input
            v-model="statsFilterModel"
            class="w-44"
            placeholder="Filter model..."
            @keyup.enter="refreshStats()"
          />
          <Button @click="refreshStats()">
            <Search class="h-3.5 w-3.5" />
            Query
          </Button>
          <Button
            variant="outline"
            class="ml-auto"
            :disabled="store.statsLoading"
            @click="refreshStats()"
          >
            <RefreshCw :class="store.statsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </Button>
        </div>

        <Alert v-if="store.statsError" variant="destructive">
          <AlertCircle class="h-4 w-4" />
          <AlertTitle>Failed to load statistics</AlertTitle>
          <AlertDescription class="break-words">{{ store.statsError }}</AlertDescription>
          <Button variant="destructive" size="sm" class="mt-2" @click="refreshStats()">
            Retry
          </Button>
        </Alert>

        <div class="rounded-lg border">
          <div
            v-if="store.statsLoading"
            class="text-muted-foreground flex items-center justify-center gap-2 py-12"
          >
            <Loader2 class="h-5 w-5 animate-spin" /> Loading...
          </div>
          <div
            v-else-if="store.stats.length === 0"
            class="text-muted-foreground py-12 text-center text-sm"
          >
            No statistics available.
          </div>
          <Table v-else>
            <TableHeader>
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
                  {{ row.total_tokens.toLocaleString() }}
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
                        (((row.call_count - row.error_count) / row.call_count) * 100).toFixed(1)
                      }}%
                    </span>
                  </template>
                  <span v-else class="text-muted-foreground">—</span>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </div>
      </TabsContent>

      <!-- ── Cache panel ─────────────────────────────────────────── -->
      <TabsContent value="cache" class="space-y-4">
        <div class="flex items-center gap-3">
          <Input
            v-model="cacheFilterProvider"
            class="w-44"
            placeholder="Filter provider..."
            @keyup.enter="refreshCache()"
          />
          <Input
            v-model="cacheFilterModel"
            class="w-44"
            placeholder="Filter model..."
            @keyup.enter="refreshCache()"
          />
          <Button @click="refreshCache()">
            <Search class="h-3.5 w-3.5" />
            Query
          </Button>
          <Button
            variant="outline"
            class="ml-auto"
            :disabled="store.cacheLoading"
            @click="refreshCache()"
          >
            <RefreshCw :class="store.cacheLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
            Refresh
          </Button>
        </div>

        <Alert v-if="store.cacheError" variant="destructive">
          <AlertCircle class="h-4 w-4" />
          <AlertTitle>Failed to load cache statistics</AlertTitle>
          <AlertDescription class="break-words">{{ store.cacheError }}</AlertDescription>
          <Button variant="destructive" size="sm" class="mt-2" @click="refreshCache()">
            Retry
          </Button>
        </Alert>

        <div class="rounded-lg border">
          <div
            v-if="store.cacheLoading"
            class="text-muted-foreground flex items-center justify-center gap-2 py-12"
          >
            <Loader2 class="h-5 w-5 animate-spin" /> Loading...
          </div>
          <div
            v-else-if="store.cache.length === 0"
            class="text-muted-foreground py-12 text-center text-sm"
          >
            No cache statistics available.
          </div>
          <Table v-else>
            <TableHeader>
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
                  {{ row.total_prompt_tokens.toLocaleString() }}
                </TableCell>
                <TableCell class="text-right tabular-nums">
                  {{ row.total_cached_tokens.toLocaleString() }}
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
                      ((row.total_cached_tokens / row.total_prompt_tokens) * 100).toFixed(1) + "%"
                    }}
                  </span>
                  <span v-else class="text-muted-foreground">—</span>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </div>
      </TabsContent>
    </Tabs>
  </div>
</template>

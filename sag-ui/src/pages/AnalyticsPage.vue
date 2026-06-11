<script setup lang="ts">
import { Search, RefreshCw, Loader2, TrendingUp, Zap, Clock } from "@lucide/vue"
import { ref, onMounted, computed } from "vue"
import { useAnalyticsStore } from "@/stores/analytics"

const store = useAnalyticsStore()
const filterProvider = ref("")
const filterModel = ref("")

onMounted(() => store.fetchStats())

async function doSearch() {
  await store.fetchStats(filterProvider.value || undefined, filterModel.value || undefined)
}

const totals = computed(() => ({
  calls: store.stats.reduce((s, r) => s + r.call_count, 0),
  tokens: store.stats.reduce((s, r) => s + r.total_tokens, 0),
  avgMs:
    store.stats.length > 0
      ? store.stats.reduce((s, r) => s + r.avg_duration_ms * r.call_count, 0) /
        Math.max(
          1,
          store.stats.reduce((s, r) => s + r.call_count, 0),
        )
      : 0,
  errors: store.stats.reduce((s, r) => s + r.error_count, 0),
}))
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Analytics</h2>
        <p class="text-muted-foreground mt-1 text-sm">调用统计与 Token 消耗分析</p>
      </div>
      <button
        @click="doSearch()"
        :disabled="store.statsLoading"
        class="hover:bg-accent inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
      >
        <RefreshCw :class="store.statsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
        Refresh
      </button>
    </div>

    <!-- Summary cards -->
    <div class="grid grid-cols-2 gap-4 lg:grid-cols-4">
      <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
        <TrendingUp class="h-5 w-5 text-blue-500" />
        <div>
          <p class="text-muted-foreground text-xs">Total Calls</p>
          <p class="text-lg font-bold tabular-nums">{{ totals.calls.toLocaleString() }}</p>
        </div>
      </div>
      <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
        <Zap class="h-5 w-5 text-amber-500" />
        <div>
          <p class="text-muted-foreground text-xs">Total Tokens</p>
          <p class="text-lg font-bold tabular-nums">{{ totals.tokens.toLocaleString() }}</p>
        </div>
      </div>
      <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
        <Clock class="h-5 w-5 text-green-500" />
        <div>
          <p class="text-muted-foreground text-xs">Avg Latency</p>
          <p class="text-lg font-bold tabular-nums">{{ totals.avgMs.toFixed(0) }} ms</p>
        </div>
      </div>
      <div class="bg-card flex items-center gap-3 rounded-lg border p-4">
        <div
          class="flex h-5 w-5 items-center justify-center rounded-full border-2 border-red-500 text-[10px] font-bold text-red-500"
        >
          !
        </div>
        <div>
          <p class="text-muted-foreground text-xs">Errors</p>
          <p
            class="text-lg font-bold tabular-nums"
            :class="totals.errors > 0 ? 'text-red-500' : ''"
          >
            {{ totals.errors }}
          </p>
        </div>
      </div>
    </div>

    <!-- Filters -->
    <div class="flex items-center gap-3">
      <input
        v-model="filterProvider"
        @keyup.enter="doSearch()"
        class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
        placeholder="Filter provider..."
      />
      <input
        v-model="filterModel"
        @keyup.enter="doSearch()"
        class="bg-background focus:ring-ring h-9 w-44 rounded-md border px-3 text-sm outline-none focus:ring-2"
        placeholder="Filter model..."
      />
      <button
        @click="doSearch()"
        class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
      >
        <Search class="h-3.5 w-3.5" />
        Query
      </button>
    </div>

    <!-- Stats table -->
    <div class="bg-card rounded-lg border">
      <div v-if="store.statsLoading" class="text-muted-foreground py-12 text-center">
        <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
      </div>
      <div
        v-else-if="store.stats.length === 0"
        class="text-muted-foreground py-12 text-center text-sm"
      >
        No statistics available.
      </div>
      <table v-else class="w-full text-sm">
        <thead>
          <tr class="bg-muted/50 border-b">
            <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Provider</th>
            <th class="text-muted-foreground px-4 py-2.5 text-left font-medium">Model</th>
            <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Calls</th>
            <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Tokens</th>
            <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Avg ms</th>
            <th class="text-muted-foreground px-4 py-2.5 text-right font-medium">Errors</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="(row, i) in store.stats"
            :key="i"
            class="hover:bg-muted/30 border-b last:border-0"
          >
            <td class="px-4 py-2.5 font-medium">{{ row.provider }}</td>
            <td class="px-4 py-2.5 font-mono text-xs">{{ row.model }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums">
              {{ row.call_count.toLocaleString() }}
            </td>
            <td class="px-4 py-2.5 text-right tabular-nums">
              {{ row.total_tokens.toLocaleString() }}
            </td>
            <td class="px-4 py-2.5 text-right tabular-nums">
              {{ row.avg_duration_ms.toFixed(0) }}
            </td>
            <td
              class="px-4 py-2.5 text-right tabular-nums"
              :class="row.error_count > 0 ? 'font-medium text-red-500' : ''"
            >
              {{ row.error_count }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

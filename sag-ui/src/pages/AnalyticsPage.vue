<script setup lang="ts">
import { ref, onMounted, computed } from 'vue'
import { useApiStore } from '@/stores/api'
import { Search, RefreshCw, Loader2, TrendingUp, Zap, Clock } from 'lucide-vue-next'

const store = useApiStore()
const filterProvider = ref('')
const filterModel = ref('')

onMounted(() => store.fetchStats())

async function doSearch() {
  await store.fetchStats(
    filterProvider.value || undefined,
    filterModel.value || undefined,
  )
}

const totals = computed(() => ({
  calls: store.stats.reduce((s, r) => s + r.call_count, 0),
  tokens: store.stats.reduce((s, r) => s + r.total_tokens, 0),
  avgMs: store.stats.length > 0
    ? store.stats.reduce((s, r) => s + r.avg_duration_ms * r.call_count, 0) / Math.max(1, store.stats.reduce((s, r) => s + r.call_count, 0))
    : 0,
  errors: store.stats.reduce((s, r) => s + r.error_count, 0),
}))
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Analytics</h2>
        <p class="text-sm text-muted-foreground mt-1">调用统计与 Token 消耗分析</p>
      </div>
      <button
        @click="doSearch()"
        :disabled="store.statsLoading"
        class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md border hover:bg-accent transition-colors"
      >
        <RefreshCw :class="store.statsLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
        Refresh
      </button>
    </div>

    <!-- Summary cards -->
    <div class="grid grid-cols-2 lg:grid-cols-4 gap-4">
      <div class="rounded-lg border bg-card p-4 flex items-center gap-3">
        <TrendingUp class="h-5 w-5 text-blue-500" />
        <div><p class="text-xs text-muted-foreground">Total Calls</p><p class="text-lg font-bold tabular-nums">{{ totals.calls.toLocaleString() }}</p></div>
      </div>
      <div class="rounded-lg border bg-card p-4 flex items-center gap-3">
        <Zap class="h-5 w-5 text-amber-500" />
        <div><p class="text-xs text-muted-foreground">Total Tokens</p><p class="text-lg font-bold tabular-nums">{{ totals.tokens.toLocaleString() }}</p></div>
      </div>
      <div class="rounded-lg border bg-card p-4 flex items-center gap-3">
        <Clock class="h-5 w-5 text-green-500" />
        <div><p class="text-xs text-muted-foreground">Avg Latency</p><p class="text-lg font-bold tabular-nums">{{ totals.avgMs.toFixed(0) }} ms</p></div>
      </div>
      <div class="rounded-lg border bg-card p-4 flex items-center gap-3">
        <div class="h-5 w-5 rounded-full border-2 border-red-500 flex items-center justify-center text-[10px] text-red-500 font-bold">!</div>
        <div><p class="text-xs text-muted-foreground">Errors</p><p class="text-lg font-bold tabular-nums" :class="totals.errors > 0 ? 'text-red-500' : ''">{{ totals.errors }}</p></div>
      </div>
    </div>

    <!-- Filters -->
    <div class="flex items-center gap-3">
      <input
        v-model="filterProvider"
        @keyup.enter="doSearch()"
        class="h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring w-44"
        placeholder="Filter provider..."
      />
      <input
        v-model="filterModel"
        @keyup.enter="doSearch()"
        class="h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring w-44"
        placeholder="Filter model..."
      />
      <button
        @click="doSearch()"
        class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md bg-primary text-primary-foreground hover:bg-primary/90 transition-colors"
      >
        <Search class="h-3.5 w-3.5" />
        Query
      </button>
    </div>

    <!-- Stats table -->
    <div class="rounded-lg border bg-card">
      <div v-if="store.statsLoading" class="py-12 text-center text-muted-foreground">
        <Loader2 class="h-6 w-6 animate-spin mx-auto mb-2" /> Loading...
      </div>
      <div v-else-if="store.stats.length === 0" class="py-12 text-center text-muted-foreground text-sm">
        No statistics available.
      </div>
      <table v-else class="w-full text-sm">
        <thead>
          <tr class="border-b bg-muted/50">
            <th class="text-left px-4 py-2.5 font-medium text-muted-foreground">Provider</th>
            <th class="text-left px-4 py-2.5 font-medium text-muted-foreground">Model</th>
            <th class="text-right px-4 py-2.5 font-medium text-muted-foreground">Calls</th>
            <th class="text-right px-4 py-2.5 font-medium text-muted-foreground">Tokens</th>
            <th class="text-right px-4 py-2.5 font-medium text-muted-foreground">Avg ms</th>
            <th class="text-right px-4 py-2.5 font-medium text-muted-foreground">Errors</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(row, i) in store.stats" :key="i" class="border-b last:border-0 hover:bg-muted/30">
            <td class="px-4 py-2.5 font-medium">{{ row.provider }}</td>
            <td class="px-4 py-2.5 font-mono text-xs">{{ row.model }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums">{{ row.call_count.toLocaleString() }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums">{{ row.total_tokens.toLocaleString() }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums">{{ row.avg_duration_ms.toFixed(0) }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums" :class="row.error_count > 0 ? 'text-red-500 font-medium' : ''">
              {{ row.error_count }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

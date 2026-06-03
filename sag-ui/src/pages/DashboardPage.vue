<script setup lang="ts">
import { onMounted } from 'vue'
import { useApiStore } from '@/stores/api'
import { Server, GitBranch, MessageSquare, AlertTriangle } from 'lucide-vue-next'

const store = useApiStore()

onMounted(async () => {
  await Promise.all([
    store.fetchProviders(),
    store.fetchRoutes(),
    store.fetchStats(),
    store.checkHealth(),
  ])
})

const cards = [
  { label: 'Providers', value: () => store.summary.providerCount, icon: Server, color: 'text-blue-500' },
  { label: 'Routes', value: () => store.summary.routeCount, icon: GitBranch, color: 'text-green-500' },
  { label: 'Total Calls', value: () => store.summary.totalCalls, icon: MessageSquare, color: 'text-purple-500' },
  { label: 'Errors', value: () => store.summary.totalErrors, icon: AlertTriangle, color: store.summary.totalErrors > 0 ? 'text-red-500' : 'text-muted-foreground' },
]
</script>

<template>
  <div class="space-y-6">
    <div>
      <h2 class="text-2xl font-bold tracking-tight">Dashboard</h2>
      <p class="text-sm text-muted-foreground mt-1">SAG AI Gateway 运行概览</p>
    </div>

    <!-- Stat cards -->
    <div class="grid grid-cols-2 lg:grid-cols-4 gap-4">
      <div v-for="card in cards" :key="card.label"
        class="rounded-lg border bg-card p-4 flex items-center gap-4 hover:shadow-md transition-shadow">
        <div class="h-10 w-10 rounded-lg bg-muted flex items-center justify-center">
          <component :is="card.icon" class="h-5 w-5" :class="card.color" />
        </div>
        <div>
          <p class="text-xs text-muted-foreground">{{ card.label }}</p>
          <p class="text-xl font-bold">{{ card.value() }}</p>
        </div>
      </div>
    </div>

    <!-- Recent stats -->
    <div class="rounded-lg border bg-card">
      <div class="p-4 border-b">
        <h3 class="font-semibold">Recent Statistics</h3>
      </div>
      <div v-if="store.stats.length === 0" class="p-8 text-center text-muted-foreground text-sm">
        No data available yet. Make some API calls to see statistics.
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
            <td class="px-4 py-2.5">{{ row.provider }}</td>
            <td class="px-4 py-2.5 font-mono text-xs">{{ row.model }}</td>
            <td class="px-4 py-2.5 text-right tabular-nums">{{ row.call_count }}</td>
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

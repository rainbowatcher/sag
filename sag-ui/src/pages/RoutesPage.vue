<script setup lang="ts">
import { Plus, RefreshCw, Loader2, GitBranch } from "lucide-vue-next"
import { ref, onMounted } from "vue"
import RouteAddForm from "@/components/routes/RouteAddForm.vue"
import { Button } from "@/components/ui/button"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const showAdd = ref(false)

onMounted(async () => {
  await Promise.all([providerStore.fetchProviders(), routeStore.fetchRoutes()])
})
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Routes</h2>
        <p class="text-muted-foreground mt-1 text-sm">管理模型路由规则与 API 端点映射</p>
      </div>
      <div class="flex items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          @click="routeStore.fetchRoutes()"
          :disabled="routeStore.routesLoading"
        >
          <RefreshCw :class="routeStore.routesLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </Button>
        <Button @click="showAdd = !showAdd" :disabled="providerStore.providers.length === 0">
          <Plus class="h-3.5 w-3.5" />
          Add Route
        </Button>
      </div>
    </div>

    <RouteAddForm
      v-if="showAdd"
      :providers="providerStore.providers"
      :provider-models="routeStore.providerModels"
      :provider-models-loading="routeStore.providerModelsLoading"
      @close="showAdd = false"
      @created="
        showAdd = false
        routeStore.fetchRoutes()
      "
    />
    <!-- Route list -->
    <div
      v-if="routeStore.routesLoading && routeStore.routes.length === 0"
      class="text-muted-foreground py-12 text-center"
    >
      <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
    </div>

    <div
      v-else-if="routeStore.routes.length === 0"
      class="text-muted-foreground py-12 text-center text-sm"
    >
      No routes configured. Add one above.
    </div>

    <div v-else class="space-y-4">
      <div
        v-for="route in routeStore.routes"
        :key="route.model_name"
        class="bg-card overflow-hidden rounded-lg border"
      >
        <div class="bg-muted/20 border-b p-4">
          <div class="flex items-center justify-between">
            <div class="flex items-center gap-3">
              <GitBranch class="text-muted-foreground h-4 w-4" />
              <span class="font-mono text-sm font-medium">{{ route.model_name }}</span>
              <span class="text-muted-foreground rounded-full border px-1.5 py-0.5 text-[10px]">{{
                route.api_endpoint
              }}</span>
            </div>
            <span class="text-muted-foreground text-xs">{{ route.targets.length }} target(s)</span>
          </div>
        </div>
        <div class="divide-y">
          <div
            v-for="(t, i) in route.targets"
            :key="i"
            class="hover:bg-muted/20 flex items-center justify-between px-4 py-3 text-sm"
          >
            <div class="flex items-center gap-2">
              <div class="bg-primary h-1.5 w-1.5 rounded-full" />
              <span class="font-medium">{{ t.provider }}</span>
              <span class="text-muted-foreground">→</span>
              <span class="font-mono text-xs">{{ t.real_model }}</span>
            </div>
            <span class="text-muted-foreground rounded-full border px-2 py-0.5 text-xs"
              >p: {{ t.priority }}</span
            >
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

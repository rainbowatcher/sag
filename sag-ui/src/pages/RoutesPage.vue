<script setup lang="ts">
import { AlertCircle, Loader2, Plus } from "@lucide/vue"
import { onMounted, ref } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import RouteAddForm from "@/components/routes/RouteAddForm.vue"
import RouteList from "@/components/routes/RouteList.vue"
import { Button } from "@/components/ui/button"
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import { useStickyTargetStore } from "@/stores/stickyTarget"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const stickyTargetStore = useStickyTargetStore()

// ── Add form ──────────────────────────────────────────────────────
const showAdd = ref(false)

async function handleRouteCreated() {
  showAdd.value = false
  await Promise.all([routeStore.fetchRoutes(), stickyTargetStore.fetchStickyTargets()])
}

const pageError = ref("")
async function refreshPage() {
  pageError.value = ""
  try {
    await Promise.all([
      providerStore.fetchProviders(),
      routeStore.fetchRoutes(),
      stickyTargetStore.fetchStickyTargets(),
    ])
  } catch (e: any) {
    pageError.value = getErrorMessage(e, "Failed to load data")
  }
}

onMounted(refreshPage)
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0 space-y-4">
      <div class="flex items-center justify-end">
        <div class="flex items-center gap-2">
          <RefreshButton :loading="routeStore.routesLoading" @click="refreshPage" />
          <Button size="sm" @click="showAdd = true">
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
        @created="handleRouteCreated"
      />
    </div>

    <div class="min-h-0 flex-1 pt-4 pb-4">
      <div
        v-if="routeStore.routesLoading && routeStore.routes.length === 0"
        class="text-muted-foreground py-12 text-center"
      >
        <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
        Loading…
      </div>

      <!-- ── Error ─────────────────────────────────────────────────── -->
      <div
        v-else-if="pageError"
        class="text-destructive flex items-center justify-center gap-2 py-12 text-sm"
      >
        <AlertCircle class="h-4 w-4" />
        {{ pageError }}
      </div>

      <!-- ── Empty ─────────────────────────────────────────────────── -->
      <div
        v-else-if="routeStore.routes.length === 0"
        class="text-muted-foreground py-12 text-center text-sm"
      >
        No routes configured. Click "Add Route" to get started.
      </div>

      <!-- ── Route List (table + edit + delete) ──────────────────── -->
      <RouteList v-else />
    </div>
  </div>
</template>

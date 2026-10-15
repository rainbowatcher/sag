<script setup lang="ts">
import { AlertCircle, Loader2, Plus, Search } from "@lucide/vue"
import { refDebounced } from "@vueuse/core"
import { onMounted, ref } from "vue"
import RefreshButton from "@/components/RefreshButton.vue"
import RouteAddForm from "@/components/routes/RouteAddForm.vue"
import RouteList from "@/components/routes/RouteList.vue"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useResponsiveModal } from "@/composables/useResponsiveModal"
import { t } from "@/lib/i18n"
import { getErrorMessage } from "@/lib/utils"
import { useActiveTargetStore } from "@/stores/activeTarget"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const activeTargetStore = useActiveTargetStore()

// Dialog on desktop, bottom Drawer on mobile.
const { Modal } = useResponsiveModal()

// ── Search / filter (debounced, shared with RouteList) ────────────
const searchInput = ref("")
const debouncedSearch = refDebounced(searchInput, 300)

// ── Add form ──────────────────────────────────────────────────────
const showAdd = ref(false)

function onAddOpenChange(v: boolean) {
  if (!v) showAdd.value = false
}

async function handleRouteCreated() {
  showAdd.value = false
  await Promise.all([routeStore.fetchRoutes(), activeTargetStore.fetchActiveTargets()])
}

const pageError = ref("")
async function refreshPage() {
  pageError.value = ""
  try {
    await Promise.all([
      providerStore.fetchProviders(),
      routeStore.fetchRoutes(),
      activeTargetStore.fetchActiveTargets(),
    ])
  } catch (e: any) {
    pageError.value = getErrorMessage(e, t("routeList.loadFailed"))
  }
}

onMounted(refreshPage)
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0 space-y-4">
      <div class="flex items-center gap-2">
        <div class="min-w-0 flex-1">
          <div class="relative">
            <Search
              class="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 z-10 h-4 w-4 -translate-y-1/2"
            />
            <Input
              v-model="searchInput"
              type="text"
              :placeholder="$t('routeList.searchPlaceholder')"
              class="h-8 pl-8"
            />
          </div>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <RefreshButton :loading="routeStore.routesLoading" @click="refreshPage" />
          <Button size="sm" @click="showAdd = true">
            <Plus class="h-3.5 w-3.5" />
            {{ $t("routeList.addRoute") }}
          </Button>
        </div>
      </div>
    </div>

    <!-- ── Add Route Dialog / Drawer ─────────────────────────────────── -->
    <component :is="Modal.Root" :open="showAdd" @update:open="onAddOpenChange">
      <component
        :is="Modal.Content"
        class="flex max-h-[85vh] flex-col overflow-hidden px-4 sm:max-w-3xl sm:px-6"
      >
        <component :is="Modal.Header" class="shrink-0">
          <component :is="Modal.Title">{{ $t("routes.addForm.title") }}</component>
          <component :is="Modal.Description">
            {{ $t("routes.addForm.description") }}
          </component>
        </component>
        <RouteAddForm
          :providers="providerStore.providers"
          :provider-models="routeStore.providerModels"
          :provider-models-loading="routeStore.providerModelsLoading"
          @close="showAdd = false"
          @created="handleRouteCreated"
        />
      </component>
    </component>

    <div class="min-h-0 flex-1 pt-4 pb-4">
      <div
        v-if="routeStore.routesLoading && routeStore.routes.length === 0"
        class="text-muted-foreground py-12 text-center"
      >
        <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" />
        {{ $t("common.loading") }}
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
        {{ $t("routeList.empty") }}
      </div>

      <!-- ── Route List (table + edit + delete) ──────────────────── -->
      <RouteList v-else :search-query="debouncedSearch" />
    </div>
  </div>
</template>

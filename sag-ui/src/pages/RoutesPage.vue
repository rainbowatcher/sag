<script setup lang="ts">
import { Plus, RefreshCw, Loader2, GitBranch, Pen, Trash2, AlertTriangle } from "lucide-vue-next"
import { ref, onMounted } from "vue"
import RouteAddForm from "@/components/routes/RouteAddForm.vue"
import RouteEditForm from "@/components/routes/RouteEditForm.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()
const showAdd = ref(false)

// ── Delete flow ──────────────────────────────────────────────────
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)
const deleteError = ref("")

function startDelete(modelName: string) {
  deleteError.value = ""
  editingName.value = null
  confirmingDelete.value = confirmingDelete.value === modelName ? null : modelName
}

async function confirmDelete(modelName: string) {
  deleting.value = true
  deleteError.value = ""
  try {
    await routeStore.deleteRoute(modelName)
    confirmingDelete.value = null
    await routeStore.fetchRoutes()
  } catch (e: any) {
    deleteError.value = e.response?.data?.error?.message ?? e.message
  } finally {
    deleting.value = false
  }
}

function cancelDelete() {
  confirmingDelete.value = null
  deleteError.value = ""
}

// ── Edit flow ────────────────────────────────────────────────────
const editingName = ref<string | null>(null)

function startEdit(route: RouteConfig) {
  confirmingDelete.value = null
  editingName.value = route.model_name
}

async function onUpdated() {
  editingName.value = null
  await routeStore.fetchRoutes()
}

onMounted(async () => {
  await Promise.all([providerStore.fetchProviders(), routeStore.fetchRoutes()])
})

const handleRouteCreated = () => {
  showAdd.value = false
  routeStore.fetchRoutes()
}
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
      @created="handleRouteCreated"
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
              <Badge variant="outline" class="rounded-full px-2 py-0.5 text-[10px] font-normal">
                {{ route.api_endpoint }}
              </Badge>
              <span
                class="rounded-full border px-1.5 py-0.5 text-[10px]"
                :class="
                  route.strategy === 'round_robin'
                    ? 'border-blue-200 text-blue-600'
                    : 'border-green-200 text-green-600'
                "
              >
                {{ route.strategy }}
              </span>
            </div>
            <div class="flex items-center gap-2">
              <span class="text-muted-foreground text-xs"
                >{{ route.targets.length }} target(s)</span
              >
              <Button
                variant="ghost"
                size="icon-sm"
                @click.stop="startEdit(route)"
                :class="editingName === route.model_name ? 'text-primary bg-primary/10' : ''"
                :title="editingName === route.model_name ? 'Cancel edit' : 'Edit route'"
                :aria-label="editingName === route.model_name ? 'Cancel edit' : 'Edit route'"
              >
                <Pen class="h-3.5 w-3.5" />
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                @click.stop="startDelete(route.model_name)"
                :class="
                  confirmingDelete === route.model_name ? 'text-destructive bg-destructive/10' : ''
                "
                :title="confirmingDelete === route.model_name ? 'Cancel delete' : 'Delete route'"
                :aria-label="
                  confirmingDelete === route.model_name ? 'Cancel delete' : 'Delete route'
                "
              >
                <Trash2 class="h-3.5 w-3.5" />
              </Button>
            </div>
          </div>
        </div>

        <!-- Inline confirm bar -->
        <div
          v-if="confirmingDelete === route.model_name"
          class="bg-destructive/5 space-y-2 border-t px-4 py-3 text-xs"
        >
          <div class="text-destructive flex items-start gap-2">
            <AlertTriangle class="mt-0.5 h-4 w-4 shrink-0" />
            <div>
              <p class="font-medium">Delete route &quot;{{ route.model_name }}&quot;?</p>
              <p class="text-muted-foreground mt-0.5">
                This will remove the routing rule permanently.
              </p>
            </div>
          </div>
          <div v-if="deleteError" class="text-destructive bg-destructive/10 rounded-md px-2 py-1.5">
            {{ deleteError }}
          </div>
          <div class="flex justify-end gap-2">
            <Button variant="outline" size="sm" @click="cancelDelete" :disabled="deleting">
              Cancel
            </Button>
            <Button
              variant="destructive"
              size="sm"
              @click="confirmDelete(route.model_name)"
              :disabled="deleting"
            >
              <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
              {{ deleting ? "Deleting…" : "Delete" }}
            </Button>
          </div>
        </div>

        <!-- Edit form -->
        <div v-if="editingName === route.model_name" class="bg-muted/20 border-t px-4 py-3">
          <RouteEditForm :route="route" @close="editingName = null" @updated="onUpdated" />
        </div>

        <!-- Route targets -->
        <div
          v-if="confirmingDelete !== route.model_name && editingName !== route.model_name"
          class="divide-y"
        >
          <div
            v-for="(t, i) in route.targets"
            :key="i"
            class="hover:bg-muted/20 flex items-center justify-between px-4 py-3 text-sm"
          >
            <div class="flex items-center gap-2">
              <div class="bg-primary h-1.5 w-1.5 rounded-full" />
              <span class="font-medium">{{ t.provider }}</span>
              <span class="text-muted-foreground">→</span>
              <span class="font-mono text-xs">{{ t.real_model || "(passthrough)" }}</span>
              <span
                v-if="t.extra && Object.keys(t.extra).length > 0"
                class="text-muted-foreground bg-muted rounded px-1.5 py-0.5 font-mono text-[10px]"
                :title="JSON.stringify(t.extra, null, 2)"
              >
                +{{ Object.keys(t.extra).length }} extra
              </span>
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

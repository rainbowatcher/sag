<script setup lang="ts">
import {
  ChevronDown,
  Trash2,
  Pen,
  AlertTriangle,
  Loader2,
  Check,
  X,
  RefreshCw,
} from "lucide-vue-next"
import { ref } from "vue"
import ProviderEditForm from "@/components/providers/ProviderEditForm.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { api, headers as authHeaders } from "@/lib/client"
import { useConnectionStore } from "@/stores/connection"
import { useProviderStore } from "@/stores/providers"
import type { ModelTestResult, Provider } from "@/types/api"

const store = useProviderStore()

const expanded = ref<Record<string, boolean>>({})
const editingProvider = ref<Provider | null>(null)
const testingModel = ref<Record<string, Record<string, boolean>>>({})
const testModelResult = ref<Record<string, Record<string, ModelTestResult[] | null>>>({})

async function testModel(providerName: string, model: string) {
  if (!testingModel.value[providerName]) testingModel.value[providerName] = {}
  if (!testModelResult.value[providerName]) testModelResult.value[providerName] = {}
  testingModel.value[providerName][model] = true
  testModelResult.value[providerName][model] = null
  try {
    const conn = useConnectionStore()
    const provider = store.providers.find((p) => p.name === providerName)
    if (!provider) return
    const keys = provider.api_keys
    const { data } = await api.post<{ results: ModelTestResult[] }>(
      "/api/model/test",
      {
        model,
        api_keys: keys,
        base_url: provider.base_url,
        provider_type: provider.provider_type,
        headers: provider.headers,
      },
      { headers: authHeaders(conn.connection?.apiKey) },
    )
    testModelResult.value[providerName][model] = data.results
  } catch (e: any) {
    testModelResult.value[providerName][model] = [
      {
        key_prefix: "error",
        success: false,
        message: e.response?.data?.error?.message ?? e.message,
      },
    ]
  } finally {
    testingModel.value[providerName][model] = false
  }
}

// ── Delete flow ──────────────────────────────────────────────────
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)
const deleteError = ref("")

function startDelete(name: string) {
  deleteError.value = ""
  confirmingDelete.value = confirmingDelete.value === name ? null : name
}

async function confirmDelete(name: string) {
  deleting.value = true
  deleteError.value = ""
  try {
    await store.deleteProvider(name)
    confirmingDelete.value = null
    delete expanded.value[name]
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
function startEdit(p: Provider) {
  confirmingDelete.value = null
  editingProvider.value = p
}

function onEditClose() {
  editingProvider.value = null
}

async function onUpdated() {
  editingProvider.value = null
  await store.fetchProviders()
}
</script>

<template>
  <div
    v-if="store.providersLoading && store.providers.length === 0"
    class="text-muted-foreground py-12 text-center"
  >
    <Loader2 class="mx-auto mb-2 h-6 w-6 animate-spin" /> Loading...
  </div>

  <div
    v-else-if="store.providers.length === 0"
    class="text-muted-foreground py-12 text-center text-sm"
  >
    No providers configured. Add one above.
  </div>

  <div v-else class="space-y-3">
    <div
      v-for="p in store.providers"
      :key="p.name"
      class="bg-card overflow-hidden rounded-lg border"
    >
      <div class="hover:bg-muted/30 flex items-center justify-between transition-colors">
        <button
          @click="expanded[p.name] = !expanded[p.name]"
          class="flex flex-1 items-center gap-3 p-4 text-left"
        >
          <div
            class="bg-primary/10 text-primary flex h-8 w-8 items-center justify-center rounded text-xs font-bold uppercase"
          >
            {{ p.name[0] }}
          </div>
          <div>
            <p class="text-sm font-medium">{{ p.name }}</p>
            <p class="text-muted-foreground text-xs">{{ p.provider_type }} · {{ p.base_url }}</p>
          </div>
        </button>
        <div class="flex items-center gap-1 pr-3">
          <Button
            variant="ghost"
            size="icon-sm"
            @click.stop="startEdit(p)"
            title="Edit provider"
            aria-label="Edit provider"
          >
            <Pen class="h-3.5 w-3.5" />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            @click.stop="startDelete(p.name)"
            :class="confirmingDelete === p.name ? 'text-destructive bg-destructive/10' : ''"
            :title="confirmingDelete === p.name ? 'Cancel delete' : 'Delete provider'"
            :aria-label="confirmingDelete === p.name ? 'Cancel delete' : 'Delete provider'"
          >
            <Trash2 class="h-3.5 w-3.5" />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            @click.stop="expanded[p.name] = !expanded[p.name]"
            :aria-label="expanded[p.name] ? 'Collapse' : 'Expand'"
          >
            <ChevronDown
              class="h-4 w-4 transition-transform"
              :class="expanded[p.name] ? 'rotate-180' : ''"
            />
          </Button>
        </div>
      </div>

      <!-- Inline confirm bar -->
      <div
        v-if="confirmingDelete === p.name"
        class="bg-destructive/5 space-y-2 border-t px-4 py-3 text-xs"
      >
        <div class="text-destructive flex items-start gap-2">
          <AlertTriangle class="mt-0.5 h-4 w-4 shrink-0" />
          <div>
            <p class="font-medium">Delete provider "{{ p.name }}"?</p>
            <p class="text-muted-foreground mt-0.5">
              Any route that uses this provider will lose its target. Routes with no remaining
              targets will be removed entirely.
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
            @click="confirmDelete(p.name)"
            :disabled="deleting"
          >
            <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
            {{ deleting ? "Deleting…" : "Delete" }}
          </Button>
        </div>
      </div>

      <!-- Expanded details -->
      <div
        v-if="expanded[p.name] && confirmingDelete !== p.name"
        class="bg-muted/20 space-y-1.5 border-t px-4 py-3 text-xs"
      >
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Base URL:</span>
          <span class="font-mono">{{ p.base_url }}</span>
        </div>
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Timeout:</span>
          <span>{{ p.timeout_secs }}s</span>
        </div>
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Key Strategy:</span>
          <span class="font-mono">{{
            p.key_strategy === "round_robin" ? "Round Robin" : "Fill First"
          }}</span>
        </div>
        <div v-if="Object.keys(p.headers ?? {}).length > 0" class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Headers:</span>
          <div class="flex flex-wrap gap-1">
            <span
              v-for="(v, k) in p.headers"
              :key="k"
              class="bg-muted rounded-full px-1.5 py-0.5 font-mono text-xs"
              >{{ k }}: {{ v }}</span
            >
          </div>
        </div>
        <div v-if="Object.keys(p.body_overrides ?? {}).length > 0" class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Body Overrides:</span>
          <div class="flex flex-wrap gap-1">
            <span
              v-for="(v, k) in p.body_overrides"
              :key="k"
              class="bg-muted rounded-full px-1.5 py-0.5 font-mono text-xs"
              >{{ k }}: {{ typeof v === "object" ? JSON.stringify(v) : String(v) }}</span
            >
          </div>
        </div>
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Models:</span>
          <span v-if="p.enabled_models.length === 0" class="text-muted-foreground text-xs"
            >All allowed</span
          >
          <div v-else class="flex flex-col gap-1">
            <div v-for="model in p.enabled_models" :key="model" class="flex items-center gap-2">
              <span class="font-mono text-xs">{{ model }}</span>
              <Button
                variant="ghost"
                size="icon-sm"
                :disabled="testingModel[p.name]?.[model]"
                @click="testModel(p.name, model)"
                title="Test model"
              >
                <Loader2 v-if="testingModel[p.name]?.[model]" class="h-3 w-3 animate-spin" />
                <RefreshCw v-else class="h-3 w-3" />
              </Button>
              <div
                v-if="testModelResult[p.name]?.[model]"
                class="flex flex-wrap items-center gap-1"
              >
                <span
                  v-for="r in testModelResult[p.name]?.[model]"
                  :key="r.key_prefix"
                  class="rounded px-1 py-0.5 text-xs"
                  :class="r.success ? 'bg-green-100 text-green-700' : 'bg-red-100 text-red-700'"
                >
                  {{ r.success ? "✓" : "✗" }} {{ r.key_prefix }}
                </span>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- Edit Drawer -->
    <ProviderEditForm
      v-if="editingProvider"
      :provider="editingProvider"
      @close="onEditClose"
      @updated="onUpdated"
    />
  </div>
</template>

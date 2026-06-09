<script setup lang="ts">
import { ChevronDown, Trash2, Pen, AlertTriangle, Loader2 } from "lucide-vue-next"
import { ref } from "vue"
import ProviderEditForm from "@/components/providers/ProviderEditForm.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { useProviderStore } from "@/stores/providers"
import type { Provider } from "@/types/api"

const store = useProviderStore()

const expanded = ref<Record<string, boolean>>({})

// ── Delete flow ──────────────────────────────────────────────────
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)
const deleteError = ref("")

function startDelete(name: string) {
  deleteError.value = ""
  editingName.value = null
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
const editingName = ref<string | null>(null)

function startEdit(p: Provider) {
  confirmingDelete.value = null
  editingName.value = p.name
}

async function onUpdated() {
  editingName.value = null
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
          <Badge variant="outline" class="rounded-full px-2 py-0.5 text-xs font-normal"
            >priority: {{ p.priority }}</Badge
          >
          <Button
            variant="ghost"
            size="icon-sm"
            @click.stop="startEdit(p)"
            :class="editingName === p.name ? 'text-primary bg-primary/10' : ''"
            :title="editingName === p.name ? 'Cancel edit' : 'Edit provider'"
            :aria-label="editingName === p.name ? 'Cancel edit' : 'Edit provider'"
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
            <p class="font-medium">Delete provider &quot;{{ p.name }}&quot;?</p>
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

      <!-- Edit form -->
      <div v-if="editingName === p.name" class="bg-muted/20 border-t px-4 py-3">
        <ProviderEditForm :provider="p" @close="editingName = null" @updated="onUpdated" />
      </div>

      <!-- Expanded details -->
      <div
        v-if="expanded[p.name] && confirmingDelete !== p.name && editingName !== p.name"
        class="bg-muted/20 space-y-1.5 border-t px-4 py-3 text-xs"
      >
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Base URL:</span>
          <span class="font-mono">{{ p.base_url }}</span>
        </div>
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Max Retries:</span>
          <span>{{ p.max_retries }}</span>
        </div>
        <div class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Timeout:</span>
          <span>{{ p.timeout_secs }}s</span>
        </div>
        <div v-if="p.supported_params.length > 0" class="flex gap-2">
          <span class="text-muted-foreground w-20 shrink-0">Params:</span>
          <div class="flex flex-wrap gap-1">
            <span
              v-for="param in p.supported_params"
              :key="param"
              class="bg-muted rounded-full px-1.5 py-0.5"
              >{{ param }}</span
            >
          </div>
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
      </div>
    </div>
  </div>
</template>

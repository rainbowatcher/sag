<script setup lang="ts">
import {
  Plus,
  RefreshCw,
  Loader2,
  X,
  ChevronDown,
  Trash2,
  Pen,
  AlertTriangle,
} from "lucide-vue-next"
import { ref, onMounted } from "vue"
import { useApiStore, type Provider } from "@/stores/api"

const store = useApiStore()
const showAdd = ref(false)
const adding = ref(false)
const error = ref("")

const form = ref({
  name: "",
  api_key: "",
  provider_type: "openai",
  base_url: "https://api.openai.com/v1",
  priority: 0,
  supported_params: "model,messages,temperature,max_tokens,stream",
})

const expanded = ref<Record<string, boolean>>({})

onMounted(() => store.fetchProviders())

async function handleAdd() {
  error.value = ""
  adding.value = true
  try {
    await store.addProvider({
      name: form.value.name,
      api_key: form.value.api_key,
      provider_type: form.value.provider_type,
      base_url: form.value.base_url || undefined,
      priority: form.value.priority,
      supported_params: form.value.supported_params
        ? form.value.supported_params
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean)
        : [],
    })
    showAdd.value = false
    form.value = {
      name: "",
      api_key: "",
      provider_type: "openai",
      base_url: "https://api.openai.com/v1",
      priority: 0,
      supported_params: "model,messages,temperature,max_tokens,stream",
    }
    await store.fetchProviders()
  } catch (e: any) {
    error.value = e.response?.data?.error?.message ?? e.message
  } finally {
    adding.value = false
  }
}

// ── Delete flow ──────────────────────────────────────────────────
// `confirmingDelete` holds the name of the provider awaiting final
// confirmation. `deleting` is true while the DELETE request is in flight.
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)
const deleteError = ref("")

// Open the inline confirm bar for a provider. Toggling off if the same
// row is clicked again — handy for cancelling with a single click.
function startDelete(name: string) {
  deleteError.value = ""
  editingName.value = null // cancel any pending edit
  confirmingDelete.value = confirmingDelete.value === name ? null : name
}

async function confirmDelete(name: string) {
  deleting.value = true
  deleteError.value = ""
  try {
    await store.deleteProvider(name)
    confirmingDelete.value = null
    // Make sure the expanded panel for the deleted provider is reset.
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
const editing = ref(false)
const editError = ref("")

const editForm = ref({
  api_key: "",
  provider_type: "openai",
  base_url: "",
  priority: 0,
  max_retries: 2,
  timeout_secs: 120,
  supported_params: "",
})

function startEdit(p: Provider) {
  editError.value = ""
  confirmingDelete.value = null // cancel any pending delete
  editingName.value = p.name
  editForm.value = {
    api_key: "",
    provider_type: p.provider_type,
    base_url: p.base_url,
    priority: p.priority,
    max_retries: p.max_retries,
    timeout_secs: p.timeout_secs,
    supported_params: p.supported_params.join(", "),
  }
}

function cancelEdit() {
  editingName.value = null
  editError.value = ""
}

async function handleUpdate(name: string) {
  editError.value = ""
  editing.value = true
  try {
    const current = store.providers.find((p) => p.name === name)
    const payload: Record<string, any> = {}
    if (editForm.value.api_key) payload.api_key = editForm.value.api_key
    if (current && editForm.value.provider_type !== current.provider_type) {
      payload.provider_type = editForm.value.provider_type
    }
    if (current && editForm.value.base_url !== current.base_url) {
      payload.base_url = editForm.value.base_url
    }
    if (current && editForm.value.priority !== current.priority) {
      payload.priority = editForm.value.priority
    }
    if (current && editForm.value.max_retries !== current.max_retries) {
      payload.max_retries = editForm.value.max_retries
    }
    if (current && editForm.value.timeout_secs !== current.timeout_secs) {
      payload.timeout_secs = editForm.value.timeout_secs
    }
    const supported = editForm.value.supported_params
      ? editForm.value.supported_params
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean)
      : []
    const origSupported = current?.supported_params ?? []
    if (JSON.stringify(supported) !== JSON.stringify(origSupported)) {
      payload.supported_params = supported
    }
    await store.updateProvider(name, payload)
    editingName.value = null
    await store.fetchProviders()
  } catch (e: any) {
    editError.value = e.response?.data?.error?.message ?? e.message
  } finally {
    editing.value = false
  }
}
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Providers</h2>
        <p class="text-muted-foreground mt-1 text-sm">管理上游 LLM 供应商配置</p>
      </div>
      <div class="flex items-center gap-2">
        <button
          @click="store.fetchProviders()"
          :disabled="store.providersLoading"
          class="hover:bg-accent inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors"
        >
          <RefreshCw :class="store.providersLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </button>
        <button
          @click="showAdd = !showAdd"
          class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-3 text-sm transition-colors"
        >
          <Plus class="h-3.5 w-3.5" />
          Add Provider
        </button>
      </div>
    </div>

    <!-- Add form -->
    <div
      v-if="showAdd"
      class="bg-card animate-in slide-in-from-top-2 space-y-4 rounded-lg border p-5"
    >
      <div class="flex items-center justify-between">
        <h3 class="font-semibold">New Provider</h3>
        <button @click="showAdd = false" class="hover:text-destructive transition-colors">
          <X class="h-4 w-4" />
        </button>
      </div>
      <div v-if="error" class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ error }}
      </div>
      <div class="grid grid-cols-2 gap-3">
        <div class="space-y-1">
          <label class="text-muted-foreground text-xs font-medium">Name *</label>
          <input
            v-model="form.name"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="my-openai"
          />
        </div>
        <div class="space-y-1">
          <label class="text-muted-foreground text-xs font-medium">Type</label>
          <select
            v-model="form.provider_type"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
          >
            <option value="openai">OpenAI</option>
            <option value="anthropic">Anthropic</option>
          </select>
        </div>
        <div class="col-span-2 space-y-1">
          <label class="text-muted-foreground text-xs font-medium">API Key *</label>
          <input
            v-model="form.api_key"
            type="password"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="sk-..."
          />
        </div>
        <div class="col-span-2 space-y-1">
          <label class="text-muted-foreground text-xs font-medium">Base URL</label>
          <input
            v-model="form.base_url"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
          />
        </div>
        <div class="space-y-1">
          <label class="text-muted-foreground text-xs font-medium">Priority</label>
          <input
            v-model.number="form.priority"
            type="number"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
          />
        </div>
        <div class="space-y-1">
          <label class="text-muted-foreground text-xs font-medium">Supported Params</label>
          <input
            v-model="form.supported_params"
            class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
            placeholder="model,messages,temperature"
          />
        </div>
      </div>
      <div class="flex justify-end">
        <button
          @click="handleAdd"
          :disabled="adding || !form.name || !form.api_key"
          class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-9 items-center gap-1.5 rounded-md px-4 text-sm transition-colors disabled:opacity-50"
        >
          <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
          {{ adding ? "Adding..." : "Create Provider" }}
        </button>
      </div>
    </div>

    <!-- Provider list -->
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
            <span class="text-muted-foreground rounded-full border px-2 py-0.5 text-xs"
              >priority: {{ p.priority }}</span
            >
            <button
              @click.stop="startEdit(p)"
              :class="
                editingName === p.name
                  ? 'text-primary bg-primary/10'
                  : 'text-muted-foreground hover:text-primary'
              "
              class="inline-flex h-7 w-7 items-center justify-center rounded-md transition-colors"
              :title="editingName === p.name ? 'Cancel edit' : 'Edit provider'"
              :aria-label="editingName === p.name ? 'Cancel edit' : 'Edit provider'"
            >
              <Pen class="h-3.5 w-3.5" />
            </button>
            <!--
              Delete trigger. Clicking once reveals an inline confirm bar
              (rendered below); clicking again cancels it. The stop
              propagation keeps the parent collapse toggle from firing.
            -->
            <button
              @click.stop="startDelete(p.name)"
              :class="
                confirmingDelete === p.name
                  ? 'text-destructive bg-destructive/10'
                  : 'text-muted-foreground hover:text-destructive'
              "
              class="inline-flex h-7 w-7 items-center justify-center rounded-md transition-colors"
              :title="confirmingDelete === p.name ? 'Cancel delete' : 'Delete provider'"
              :aria-label="confirmingDelete === p.name ? 'Cancel delete' : 'Delete provider'"
            >
              <Trash2 class="h-3.5 w-3.5" />
            </button>
            <button
              @click.stop="expanded[p.name] = !expanded[p.name]"
              class="text-muted-foreground hover:bg-muted inline-flex h-7 w-7 items-center justify-center rounded-md transition-colors"
              :aria-label="expanded[p.name] ? 'Collapse' : 'Expand'"
            >
              <ChevronDown
                class="h-4 w-4 transition-transform"
                :class="expanded[p.name] ? 'rotate-180' : ''"
              />
            </button>
          </div>
        </div>
        <!--
          Inline confirm bar. Renders only for the row whose trash was
          clicked. Surfaces the cascade side-effect to the user so they
          understand what will happen, and lets them back out with one
          click.
        -->
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
            <button
              @click="cancelDelete"
              :disabled="deleting"
              class="hover:bg-accent h-7 rounded-md border px-3 text-xs transition-colors disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              @click="confirmDelete(p.name)"
              :disabled="deleting"
              class="bg-destructive text-destructive-foreground hover:bg-destructive/90 inline-flex h-7 items-center gap-1.5 rounded-md px-3 text-xs transition-colors disabled:opacity-50"
            >
              <Loader2 v-if="deleting" class="h-3 w-3 animate-spin" />
              {{ deleting ? "Deleting…" : "Delete" }}
            </button>
          </div>
        </div>
        <!-- Edit form -->
        <div v-if="editingName === p.name" class="bg-muted/20 space-y-3 border-t px-4 py-3">
          <div v-if="editError" class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
            {{ editError }}
          </div>
          <div class="grid grid-cols-2 gap-3">
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Name</label>
              <input
                :value="p.name"
                disabled
                class="bg-muted text-muted-foreground h-9 w-full rounded-md border px-3 text-sm"
              />
            </div>
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Type</label>
              <select
                v-model="editForm.provider_type"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
              >
                <option value="openai">OpenAI</option>
                <option value="anthropic">Anthropic</option>
              </select>
            </div>
            <div class="col-span-2 space-y-1">
              <label class="text-muted-foreground text-xs font-medium"
                >API Key (leave blank to keep current)</label
              >
              <input
                v-model="editForm.api_key"
                type="password"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
                placeholder="Leave blank to keep current"
              />
            </div>
            <div class="col-span-2 space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Base URL</label>
              <input
                v-model="editForm.base_url"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
              />
            </div>
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Priority</label>
              <input
                v-model.number="editForm.priority"
                type="number"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
              />
            </div>
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Max Retries</label>
              <input
                v-model.number="editForm.max_retries"
                type="number"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
              />
            </div>
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Timeout (secs)</label>
              <input
                v-model.number="editForm.timeout_secs"
                type="number"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
              />
            </div>
            <div class="space-y-1">
              <label class="text-muted-foreground text-xs font-medium">Supported Params</label>
              <input
                v-model="editForm.supported_params"
                class="bg-background focus:ring-ring h-9 w-full rounded-md border px-3 text-sm outline-none focus:ring-2"
                placeholder="model,messages,temperature"
              />
            </div>
          </div>
          <div class="flex justify-end gap-2">
            <button
              @click="cancelEdit"
              :disabled="editing"
              class="hover:bg-accent h-8 rounded-md border px-3 text-xs transition-colors disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              @click="handleUpdate(p.name)"
              :disabled="editing"
              class="bg-primary text-primary-foreground hover:bg-primary/90 inline-flex h-8 items-center gap-1.5 rounded-md px-3 text-xs transition-colors disabled:opacity-50"
            >
              <Loader2 v-if="editing" class="h-3 w-3 animate-spin" />
              {{ editing ? "Saving..." : "Save" }}
            </button>
          </div>
        </div>
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
        </div>
      </div>
    </div>
  </div>
</template>

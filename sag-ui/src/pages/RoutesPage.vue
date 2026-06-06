<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { useApiStore } from '@/stores/api'
import { Plus, RefreshCw, Loader2, X, GitBranch, ChevronsUpDown, Check } from 'lucide-vue-next'
import {
  Combobox,
  ComboboxAnchor,
  ComboboxInput,
  ComboboxTrigger,
  ComboboxList,
  ComboboxItem,
  ComboboxEmpty,
} from '@/components/ui/combobox'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group'

const store = useApiStore()
const showAdd = ref(false)
const adding = ref(false)
const error = ref('')
const open = ref(false)

const form = ref({
  provider_name: '',
  model_name: '',
  real_model: '',
  api_endpoint: '/v1/chat/completions',
  priority: 0,
})

onMounted(async () => {
  await Promise.all([store.fetchProviders(), store.fetchRoutes()])
})

watch(() => form.value.provider_name, (name) => {
  form.value.real_model = ''
  if (name) {
    store.fetchProviderModels(name)
  }
})

async function handleAdd() {
  error.value = ''
  adding.value = true
  try {
    const payload: Record<string, any> = {
      model_name: form.value.model_name,
      api_endpoint: form.value.api_endpoint || undefined,
      priority: form.value.priority,
    }
    if (form.value.real_model) {
      payload.real_model = form.value.real_model
    }
    await store.addRoute(form.value.provider_name, payload)
    showAdd.value = false
    form.value = { provider_name: '', model_name: '', real_model: '', api_endpoint: '/v1/chat/completions', priority: 0 }
    await store.fetchRoutes()
  } catch (e: any) {
    error.value = e.response?.data?.error?.message ?? e.message
  } finally {
    adding.value = false
  }
}

const endpointPresets = [
  { label: 'Chat Completions', value: '/v1/chat/completions' },
  { label: 'Responses (GPT-5)', value: '/v1/responses' },
  { label: 'Image Generation', value: '/v1/images/generations' },
]
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Routes</h2>
        <p class="text-sm text-muted-foreground mt-1">管理模型路由规则与 API 端点映射</p>
      </div>
      <div class="flex items-center gap-2">
        <button
          @click="store.fetchRoutes()"
          :disabled="store.routesLoading"
          class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md border hover:bg-accent transition-colors"
        >
          <RefreshCw :class="store.routesLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </button>
        <button
          @click="showAdd = !showAdd"
          :disabled="store.providers.length === 0"
          class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md bg-primary text-primary-foreground hover:bg-primary/90 disabled:opacity-50 transition-colors"
        >
          <Plus class="h-3.5 w-3.5" />
          Add Route
        </button>
      </div>
    </div>

    <!-- Add form -->
    <div v-if="showAdd" class="rounded-lg border bg-card p-5 space-y-4">
      <div class="flex items-center justify-between">
        <h3 class="font-semibold">New Route</h3>
        <button @click="showAdd = false" class="hover:text-destructive transition-colors"><X class="h-4 w-4" /></button>
      </div>
      <div v-if="error" class="text-sm text-red-500 bg-red-50 rounded-md px-3 py-2">{{ error }}</div>
      <div class="grid grid-cols-2 gap-3">
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Provider *</label>
          <select v-model="form.provider_name" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring">
            <option value="" disabled>Select provider...</option>
            <option v-for="p in store.providers" :key="p.name" :value="p.name">{{ p.name }} ({{ p.provider_type }})</option>
          </select>
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Priority</label>
          <input v-model.number="form.priority" type="number" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" />
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Model Name *</label>
          <input v-model="form.model_name" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" placeholder="gpt-4o" />
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Real Model</label>
          <Combobox v-model="form.real_model" v-model:open="open">
            <ComboboxAnchor class="w-full">
              <ComboboxInput
                class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring"
                placeholder="Select or type a model..."
              />
              <ComboboxTrigger>
                <ChevronsUpDown class="h-4 w-4 shrink-0 opacity-50" />
              </ComboboxTrigger>
            </ComboboxAnchor>
            <ComboboxList class="w-full">
              <ComboboxEmpty>No models found</ComboboxEmpty>
              <ComboboxItem v-if="!form.real_model" :value="''" @click="open = false">
                <span class="text-muted-foreground text-xs">Passthrough (use model name)</span>
                <Check v-if="form.real_model === ''" class="ml-auto h-4 w-4" />
              </ComboboxItem>
              <div v-if="store.providerModelsLoading && !store.providerModels[form.provider_name]" class="px-2 py-4 text-center text-xs text-muted-foreground">Loading models...</div>
              <ComboboxItem
                v-for="m in store.providerModels[form.provider_name] ?? []"
                :key="m"
                :value="m"
              >
                {{ m }}
                <Check v-if="form.real_model === m" class="ml-auto h-4 w-4" />
              </ComboboxItem>
            </ComboboxList>
          </Combobox>
          <p class="text-[10px] text-muted-foreground mt-0.5">Leave empty to use the client-requested model name as-is</p>
        </div>
        <div class="col-span-2 space-y-1">
          <label class="text-xs font-medium text-muted-foreground">API Endpoint</label>
          <ToggleGroup v-model="form.api_endpoint" type="single" variant="outline" size="sm" class="justify-start">
            <ToggleGroupItem v-for="ep in endpointPresets" :key="ep.value" :value="ep.value">
              {{ ep.label }}
            </ToggleGroupItem>
          </ToggleGroup>
        </div>
      </div>
      <div class="flex justify-end">
        <button
          @click="handleAdd"
          :disabled="adding || !form.provider_name || !form.model_name"
          class="inline-flex items-center gap-1.5 h-9 px-4 rounded-md bg-primary text-primary-foreground text-sm hover:bg-primary/90 disabled:opacity-50 transition-colors"
        >
          <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
          {{ adding ? 'Adding...' : 'Create Route' }}
        </button>
      </div>
    </div>

    <!-- Route list -->
    <div v-if="store.routesLoading && store.routes.length === 0" class="py-12 text-center text-muted-foreground">
      <Loader2 class="h-6 w-6 animate-spin mx-auto mb-2" /> Loading...
    </div>

    <div v-else-if="store.routes.length === 0" class="py-12 text-center text-muted-foreground text-sm">
      No routes configured. Add one above.
    </div>

    <div v-else class="space-y-4">
      <div v-for="route in store.routes" :key="route.model_name" class="rounded-lg border bg-card overflow-hidden">
        <div class="p-4 border-b bg-muted/20">
          <div class="flex items-center justify-between">
            <div class="flex items-center gap-3">
              <GitBranch class="h-4 w-4 text-muted-foreground" />
              <span class="font-mono text-sm font-medium">{{ route.model_name }}</span>
              <span class="text-[10px] rounded-full border px-1.5 py-0.5 text-muted-foreground">{{ route.api_endpoint }}</span>
            </div>
            <span class="text-xs text-muted-foreground">{{ route.targets.length }} target(s)</span>
          </div>
        </div>
        <div class="divide-y">
          <div v-for="(t, i) in route.targets" :key="i" class="px-4 py-3 flex items-center justify-between text-sm hover:bg-muted/20">
            <div class="flex items-center gap-2">
              <div class="h-1.5 w-1.5 rounded-full bg-primary" />
              <span class="font-medium">{{ t.provider }}</span>
              <span class="text-muted-foreground">→</span>
              <span class="font-mono text-xs">{{ t.real_model }}</span>
            </div>
            <span class="text-xs text-muted-foreground rounded-full border px-2 py-0.5">p: {{ t.priority }}</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

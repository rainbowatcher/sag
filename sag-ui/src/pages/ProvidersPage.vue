<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useApiStore } from '@/stores/api'
import { Plus, RefreshCw, Loader2, X, ChevronDown } from 'lucide-vue-next'

const store = useApiStore()
const showAdd = ref(false)
const adding = ref(false)
const error = ref('')

const form = ref({
  name: '',
  api_key: '',
  provider_type: 'openai',
  base_url: 'https://api.openai.com/v1',
  priority: 0,
  supported_params: 'model,messages,temperature,max_tokens,stream',
})

const expanded = ref<Record<string, boolean>>({})

onMounted(() => store.fetchProviders())

async function handleAdd() {
  error.value = ''
  adding.value = true
  try {
    await store.addProvider({
      name: form.value.name,
      api_key: form.value.api_key,
      provider_type: form.value.provider_type,
      base_url: form.value.base_url || undefined,
      priority: form.value.priority,
      supported_params: form.value.supported_params
        ? form.value.supported_params.split(',').map(s => s.trim()).filter(Boolean)
        : [],
    })
    showAdd.value = false
    form.value = { name: '', api_key: '', provider_type: 'openai', base_url: 'https://api.openai.com/v1', priority: 10, supported_params: 'model,messages,temperature,max_tokens,stream' }
    await store.fetchProviders()
  } catch (e: any) {
    error.value = e.response?.data?.error?.message ?? e.message
  } finally {
    adding.value = false
  }
}
</script>

<template>
  <div class="space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h2 class="text-2xl font-bold tracking-tight">Providers</h2>
        <p class="text-sm text-muted-foreground mt-1">管理上游 LLM 供应商配置</p>
      </div>
      <div class="flex items-center gap-2">
        <button
          @click="store.fetchProviders()"
          :disabled="store.providersLoading"
          class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md border hover:bg-accent transition-colors"
        >
          <RefreshCw :class="store.providersLoading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
          Refresh
        </button>
        <button
          @click="showAdd = !showAdd"
          class="inline-flex items-center gap-1.5 h-9 px-3 text-sm rounded-md bg-primary text-primary-foreground hover:bg-primary/90 transition-colors"
        >
          <Plus class="h-3.5 w-3.5" />
          Add Provider
        </button>
      </div>
    </div>

    <!-- Add form -->
    <div v-if="showAdd" class="rounded-lg border bg-card p-5 space-y-4 animate-in slide-in-from-top-2">
      <div class="flex items-center justify-between">
        <h3 class="font-semibold">New Provider</h3>
        <button @click="showAdd = false" class="hover:text-destructive transition-colors"><X class="h-4 w-4" /></button>
      </div>
      <div v-if="error" class="text-sm text-red-500 bg-red-50 rounded-md px-3 py-2">{{ error }}</div>
      <div class="grid grid-cols-2 gap-3">
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Name *</label>
          <input v-model="form.name" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" placeholder="my-openai" />
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Type</label>
          <select v-model="form.provider_type" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring">
            <option value="openai">OpenAI</option>
            <option value="anthropic">Anthropic</option>
          </select>
        </div>
        <div class="col-span-2 space-y-1">
          <label class="text-xs font-medium text-muted-foreground">API Key *</label>
          <input v-model="form.api_key" type="password" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" placeholder="sk-..." />
        </div>
        <div class="col-span-2 space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Base URL</label>
          <input v-model="form.base_url" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" />
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Priority</label>
          <input v-model.number="form.priority" type="number" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" />
        </div>
        <div class="space-y-1">
          <label class="text-xs font-medium text-muted-foreground">Supported Params</label>
          <input v-model="form.supported_params" class="w-full h-9 rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring" placeholder="model,messages,temperature" />
        </div>
      </div>
      <div class="flex justify-end">
        <button
          @click="handleAdd"
          :disabled="adding || !form.name || !form.api_key"
          class="inline-flex items-center gap-1.5 h-9 px-4 rounded-md bg-primary text-primary-foreground text-sm hover:bg-primary/90 disabled:opacity-50 transition-colors"
        >
          <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
          {{ adding ? 'Adding...' : 'Create Provider' }}
        </button>
      </div>
    </div>

    <!-- Provider list -->
    <div v-if="store.providersLoading && store.providers.length === 0" class="py-12 text-center text-muted-foreground">
      <Loader2 class="h-6 w-6 animate-spin mx-auto mb-2" /> Loading...
    </div>

    <div v-else-if="store.providers.length === 0" class="py-12 text-center text-muted-foreground text-sm">
      No providers configured. Add one above.
    </div>

    <div v-else class="space-y-3">
      <div v-for="p in store.providers" :key="p.name" class="rounded-lg border bg-card overflow-hidden">
        <button @click="expanded[p.name] = !expanded[p.name]" class="w-full flex items-center justify-between p-4 text-left hover:bg-muted/30 transition-colors">
          <div class="flex items-center gap-3">
            <div class="h-8 w-8 rounded bg-primary/10 flex items-center justify-center text-primary text-xs font-bold uppercase">{{ p.name[0] }}</div>
            <div>
              <p class="font-medium text-sm">{{ p.name }}</p>
              <p class="text-xs text-muted-foreground">{{ p.provider_type }} · {{ p.base_url }}</p>
            </div>
          </div>
          <div class="flex items-center gap-3 text-xs text-muted-foreground">
            <span class="rounded-full border px-2 py-0.5">priority: {{ p.priority }}</span>
            <ChevronDown class="h-4 w-4 transition-transform" :class="expanded[p.name] ? 'rotate-180' : ''" />
          </div>
        </button>
        <div v-if="expanded[p.name]" class="border-t px-4 py-3 bg-muted/20 text-xs space-y-1.5">
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
              <span v-for="param in p.supported_params" :key="param" class="rounded-full bg-muted px-1.5 py-0.5">{{ param }}</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

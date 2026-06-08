<script setup lang="ts">
import { Plus, RefreshCw } from "lucide-vue-next"
import { ref, onMounted } from "vue"
import ProviderAddForm from "@/components/providers/ProviderAddForm.vue"
import ProviderList from "@/components/providers/ProviderList.vue"
import { useProviderStore } from "@/stores/providers"

const store = useProviderStore()
const showAdd = ref(false)

onMounted(() => store.fetchProviders())

async function onCreated() {
  showAdd.value = false
  await store.fetchProviders()
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

    <ProviderAddForm v-if="showAdd" @close="showAdd = false" @created="onCreated" />

    <ProviderList />
  </div>
</template>

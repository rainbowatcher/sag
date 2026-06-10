<script setup lang="ts">
import { onMounted } from "vue"
import ProviderHeader from "@/components/providers/ProviderHeader.vue"
import ProviderList from "@/components/providers/ProviderList.vue"
import { useProviderStore } from "@/stores/providers"

const providerStore = useProviderStore()

onMounted(() => {
  providerStore.fetchProviders()
})
</script>

<template>
  <div class="space-y-6">
    <ProviderHeader />
    <ProviderList />
  </div>
</template>

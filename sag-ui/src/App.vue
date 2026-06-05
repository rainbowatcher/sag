<script setup lang="ts">
import { RouterView } from 'vue-router'
import { useApiStore } from '@/stores/api'
import { Loader2 } from 'lucide-vue-next'

const store = useApiStore()
</script>

<template>
  <!-- Loading screen during auto-connect validation -->
  <div v-if="store.validating" class="min-h-screen flex items-center justify-center bg-muted/30">
    <div class="text-center space-y-3">
      <Loader2 class="h-8 w-8 animate-spin text-primary mx-auto" />
      <p class="text-sm text-muted-foreground">Connecting to gateway...</p>
    </div>
  </div>

  <RouterView v-else />
</template>

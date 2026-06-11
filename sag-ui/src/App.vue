<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { RouterView } from "vue-router"
import { useConnectionStore } from "@/stores/connection"

const store = useConnectionStore()
</script>

<template>
  <!-- Loading screen during auto-connect validation -->
  <div v-if="store.validating" class="bg-muted/30 flex min-h-screen items-center justify-center">
    <div class="space-y-3 text-center">
      <Loader2 class="text-primary mx-auto h-8 w-8 animate-spin" />
      <p class="text-muted-foreground text-sm">Connecting to gateway...</p>
    </div>
  </div>

  <RouterView v-else />
</template>

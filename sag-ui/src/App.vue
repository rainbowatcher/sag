<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { RouterView } from "vue-router"
import { Toaster } from "@/components/ui/sonner"
import { useConnectionStore } from "@/stores/connection"
import { TooltipProvider } from "./components/ui/tooltip"

const store = useConnectionStore()
</script>

<template>
  <TooltipProvider :delay-duration="1200">
    <!-- Loading overlay during auto-connect validation.
       Must NOT use v-if/v-else that toggles <RouterView> mount state,
       because the navigation guard may still be pending when validating
       flips back to false, producing a null vnode and crashing the app. -->
    <div
      v-if="store.validating"
      class="bg-muted/30 fixed inset-0 z-50 flex items-center justify-center"
    >
      <div class="space-y-3 text-center">
        <Loader2 class="text-primary mx-auto h-8 w-8 animate-spin" />
        <p class="text-muted-foreground text-sm">Connecting to gateway...</p>
      </div>
    </div>

    <RouterView />
    <Toaster theme="system" position="top-center" expand />
  </TooltipProvider>
</template>

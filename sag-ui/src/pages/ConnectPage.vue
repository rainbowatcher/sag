<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useApiStore } from '@/stores/api'
import { Loader2, Plug, Server, Key, AlertCircle } from 'lucide-vue-next'

const store = useApiStore()
const route = useRoute()
const router = useRouter()

const gatewayUrl = ref('http://127.0.0.1:3000')
const apiKey = ref('')
const error = ref('')

// Pre-fill URL from saved connection (if any) or from query param
onMounted(() => {
  store.loadConnection()
  if (store.connection) {
    gatewayUrl.value = store.connection.gatewayUrl
    apiKey.value = store.connection.apiKey
  }
  if (route.query.reason === 'expired') {
    error.value = 'Your session has expired. Please reconnect.'
  }
})

async function handleConnect() {
  error.value = ''
  const url = gatewayUrl.value.replace(/\/+$/, '') // strip trailing slash

  const err = await store.connect(url, apiKey.value.trim())
  if (err) {
    error.value = err
    return
  }

  router.replace('/')
}
</script>

<template>
  <div class="min-h-screen flex items-center justify-center bg-muted/30 p-4">
    <div class="w-full max-w-sm">
      <!-- Logo / Header -->
      <div class="text-center mb-8">
        <div class="h-12 w-12 rounded-xl bg-primary flex items-center justify-center text-primary-foreground text-lg font-bold mx-auto mb-3 shadow-sm">
          S
        </div>
        <h1 class="text-xl font-bold tracking-tight">SAG Gateway</h1>
        <p class="text-sm text-muted-foreground mt-1">Connect to your AI gateway</p>
      </div>

      <!-- Form card -->
      <form @submit.prevent="handleConnect" class="rounded-xl border bg-card shadow-sm p-6 space-y-4">
        <!-- Gateway URL -->
        <div class="space-y-1.5">
          <label class="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Server class="h-3.5 w-3.5" />
            Gateway URL
          </label>
          <input
            v-model="gatewayUrl"
            type="text"
            placeholder="http://127.0.0.1:3000"
            :disabled="store.connecting"
            class="w-full h-10 rounded-md border bg-background px-3 text-sm outline-none font-mono placeholder:text-muted-foreground/50 focus:ring-2 focus:ring-ring transition-shadow disabled:opacity-50"
          />
        </div>

        <!-- API Key -->
        <div class="space-y-1.5">
          <label class="text-xs font-medium text-muted-foreground flex items-center gap-1.5">
            <Key class="h-3.5 w-3.5" />
            API Key
          </label>
          <input
            v-model="apiKey"
            type="password"
            placeholder="Enter gateway API key..."
            :disabled="store.connecting"
            class="w-full h-10 rounded-md border bg-background px-3 text-sm outline-none placeholder:text-muted-foreground/50 focus:ring-2 focus:ring-ring transition-shadow disabled:opacity-50"
          />
        </div>

        <!-- Error -->
        <div
          v-if="error"
          class="flex items-start gap-2 rounded-md bg-destructive/10 px-3 py-2.5 text-xs text-destructive"
        >
          <AlertCircle class="h-3.5 w-3.5 mt-0.5 shrink-0" />
          <span>{{ error }}</span>
        </div>

        <!-- Connect button -->
        <button
          type="submit"
          :disabled="store.connecting || !gatewayUrl || !apiKey"
          class="w-full h-10 rounded-md bg-primary text-primary-foreground text-sm font-medium flex items-center justify-center gap-2 hover:bg-primary/90 disabled:opacity-50 transition-colors"
        >
          <Loader2 v-if="store.connecting" class="h-4 w-4 animate-spin" />
          <Plug v-else class="h-4 w-4" />
          {{ store.connecting ? 'Connecting...' : 'Connect' }}
        </button>
      </form>

      <p class="text-[10px] text-muted-foreground text-center mt-4">
        Configured in <code class="text-[10px] bg-muted px-1 rounded">gateway.toml</code> → <code class="text-[10px] bg-muted px-1 rounded">[gateway_auth]</code>
      </p>
    </div>
  </div>
</template>

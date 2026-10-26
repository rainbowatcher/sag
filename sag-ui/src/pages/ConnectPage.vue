<script setup lang="ts">
import { Loader2, Plug, Server, Key, AlertCircle, Info } from "@lucide/vue"
import { ref, onMounted } from "vue"
import { useRoute, useRouter } from "vue-router"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { useConnectionStore } from "@/stores/connection"

const store = useConnectionStore()
const route = useRoute()
const router = useRouter()

const gatewayUrl = ref("http://127.0.0.1:37210")
const apiKey = ref("")
const error = ref("")

// Pre-fill URL from saved connection (if any) or from query param
onMounted(() => {
  store.loadConnection()
  if (store.connection) {
    gatewayUrl.value = store.connection.gatewayUrl
    apiKey.value = store.connection.apiKey
  }
  if (route.query.reason === "expired") {
    error.value = t("connect.sessionExpired")
  }
})

async function handleConnect() {
  error.value = ""
  const url = gatewayUrl.value.replace(/\/+$/, "") // strip trailing slash

  const err = await store.connect(url, apiKey.value.trim())
  if (err) {
    error.value = err
    return
  }

  router.replace("/")
}
</script>

<template>
  <div class="bg-muted/30 flex min-h-screen items-center justify-center p-4">
    <div class="w-full max-w-sm">
      <!-- Logo / Header -->
      <div class="mb-8 text-center">
        <div
          class="bg-primary text-primary-foreground mx-auto mb-3 flex h-12 w-12 items-center justify-center rounded-xl text-lg font-bold shadow-sm"
        >
          S
        </div>
        <h1 class="flex items-center justify-center gap-1.5 text-xl font-bold tracking-tight">
          SAG Gateway
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              {{ $t("connect.subtitle") }}
              <br />
              {{ $t("connect.configuredIn")
              }}<code class="bg-muted rounded px-1 text-[10px]">gateway.toml</code
              >{{ $t("connect.configuredArrow")
              }}<code class="bg-muted rounded px-1 text-[10px]">[gateway_auth]</code>
            </TooltipContent>
          </Tooltip>
        </h1>
      </div>

      <!-- Form card -->
      <form
        @submit.prevent="handleConnect"
        class="bg-card space-y-4 rounded-xl border p-6 shadow-sm"
      >
        <!-- Gateway URL -->
        <div class="space-y-1.5">
          <label class="text-muted-foreground flex items-center gap-1.5 text-xs font-medium">
            <Server class="h-3.5 w-3.5" />
            {{ $t("connect.gatewayUrl") }}
          </label>
          <input
            v-model="gatewayUrl"
            type="text"
            placeholder="http://127.0.0.1:37210"
            :disabled="store.connecting"
            class="bg-background placeholder:text-muted-foreground/50 focus:ring-ring h-10 w-full rounded-md border px-3 font-mono text-sm transition-shadow outline-none focus:ring-2 disabled:opacity-50"
          />
        </div>

        <!-- API Key -->
        <div class="space-y-1.5">
          <label class="text-muted-foreground flex items-center gap-1.5 text-xs font-medium">
            <Key class="h-3.5 w-3.5" />
            {{ $t("connect.apiKey") }}
          </label>
          <input
            v-model="apiKey"
            type="password"
            :placeholder="$t('connect.apiKeyPlaceholder')"
            :disabled="store.connecting"
            class="bg-background placeholder:text-muted-foreground/50 focus:ring-ring h-10 w-full rounded-md border px-3 text-sm transition-shadow outline-none focus:ring-2 disabled:opacity-50"
          />
        </div>

        <!-- Error -->
        <div
          v-if="error"
          class="bg-destructive/10 text-destructive flex items-start gap-2 rounded-md px-3 py-2.5 text-xs"
        >
          <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{{ error }}</span>
        </div>

        <!-- Connect button -->
        <button
          type="submit"
          :disabled="store.connecting || !gatewayUrl || !apiKey"
          class="bg-primary text-primary-foreground hover:bg-primary/90 flex h-10 w-full items-center justify-center gap-2 rounded-md text-sm font-medium transition-colors disabled:opacity-50"
        >
          <Loader2 v-if="store.connecting" class="h-4 w-4 animate-spin" />
          <Plug v-else class="h-4 w-4" />
          {{ store.connecting ? $t("connect.connecting") : $t("connect.connect") }}
        </button>
      </form>
    </div>
  </div>
</template>

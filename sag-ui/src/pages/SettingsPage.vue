<script setup lang="ts">
import { AlertCircle, Globe, Loader2, Save, Trash2 } from "@lucide/vue"
import { onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { useSettingsStore } from "@/stores/settings"

const store = useSettingsStore()

const proxyUrl = ref("")
const formError = ref("")

// Only HTTP/HTTPS is supported by the gateway — never promise SOCKS.
function validateProxyUrl(url: string): string | null {
  if (!url.trim()) return null
  let parsed: URL
  try {
    parsed = new URL(url)
  } catch {
    return "Invalid URL. Use the form http://host:port or https://host:port."
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return "Only HTTP/HTTPS proxy URLs are supported (SOCKS is not supported)."
  }
  return null
}

onMounted(async () => {
  await store.fetchSettings()
  proxyUrl.value = store.proxyUrl ?? ""
})

async function handleSave() {
  formError.value = ""
  const value = proxyUrl.value.trim()
  const validationError = validateProxyUrl(value)
  if (validationError) {
    formError.value = validationError
    return
  }
  try {
    await store.saveSettings({ proxy_url: value || null })
    proxyUrl.value = store.proxyUrl ?? ""
    toast.success("Proxy settings saved")
  } catch {
    // store.error already holds the backend message
  }
}

async function handleClear() {
  formError.value = ""
  try {
    await store.saveSettings({ proxy_url: null })
    proxyUrl.value = ""
    toast.success("Proxy settings cleared")
  } catch {
    // store.error already holds the backend message
  }
}
</script>

<template>
  <div class="flex h-full flex-col gap-4">
    <div>
      <h1 class="text-lg font-semibold">Settings</h1>
      <p class="text-muted-foreground text-sm">Gateway-level configuration.</p>
    </div>

    <Card class="max-w-2xl">
      <CardHeader>
        <CardTitle class="flex items-center gap-2">
          <Globe class="h-4 w-4" />
          Global Proxy URL
        </CardTitle>
        <CardDescription>
          The gateway-level HTTP/HTTPS proxy used by providers whose proxy mode is
          <code>inherit</code> or <code>enabled</code>. Only HTTP/HTTPS is supported — SOCKS is not.
        </CardDescription>
      </CardHeader>
      <CardContent class="space-y-3">
        <div v-if="store.loading" class="text-muted-foreground flex items-center gap-2 text-xs">
          <Loader2 class="h-3.5 w-3.5 animate-spin" />
          Loading settings...
        </div>

        <Input
          v-model="proxyUrl"
          :disabled="store.loading || store.saving"
          placeholder="http://proxy.example:8080"
          class="font-mono"
          @keydown.enter.prevent="handleSave"
        />

        <div
          v-if="formError || store.error"
          class="bg-destructive/10 text-destructive flex items-start gap-2 rounded-md px-3 py-2 text-xs"
        >
          <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{{ formError || store.error }}</span>
        </div>

        <div class="flex items-center gap-2">
          <Button :disabled="store.loading || store.saving" @click="handleSave">
            <Loader2 v-if="store.saving" class="h-3.5 w-3.5 animate-spin" />
            <Save v-else class="h-3.5 w-3.5" />
            Save
          </Button>
          <Button
            variant="outline"
            :disabled="store.loading || store.saving || !store.proxyUrl"
            @click="handleClear"
          >
            <Trash2 class="h-3.5 w-3.5" />
            Clear
          </Button>
        </div>
      </CardContent>
    </Card>
  </div>
</template>

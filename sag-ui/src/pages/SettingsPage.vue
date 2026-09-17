<script setup lang="ts">
import {
  AlertCircle,
  Eye,
  EyeOff,
  Globe,
  KeyRound,
  Loader2,
  Plus,
  Save,
  ShieldCheck,
  ShieldOff,
  Trash2,
  X,
} from "@lucide/vue"
import { onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import RefreshButton from "@/components/RefreshButton.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { useSettingsStore } from "@/stores/settings"

const store = useSettingsStore()

// ── Proxy form ────────────────────────────────────────────────────
const proxyUrl = ref("")
const proxyError = ref("")
const proxySaving = ref(false)

// ── API keys form ─────────────────────────────────────────────────
const apiKeys = ref<string[]>([])
const keysError = ref("")
const keysSaving = ref(false)
const revealKeys = ref<Record<number, boolean>>({})

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

function syncFormsFromStore() {
  proxyUrl.value = store.proxyUrl ?? ""
  apiKeys.value = [...store.apiKeys]
  revealKeys.value = {}
}

async function refreshPage() {
  proxyError.value = ""
  keysError.value = ""
  await store.fetchSettings()
  syncFormsFromStore()
}

onMounted(refreshPage)

async function handleProxySave() {
  proxyError.value = ""
  keysError.value = ""
  const value = proxyUrl.value.trim()
  const validationError = validateProxyUrl(value)
  if (validationError) {
    proxyError.value = validationError
    return
  }
  proxySaving.value = true
  try {
    // Omit api_keys so a proxy-only save never touches the auth config.
    await store.saveSettings({ proxy_url: value || null })
    proxyUrl.value = store.proxyUrl ?? ""
    toast.success("Proxy settings saved")
  } catch (e: any) {
    proxyError.value = store.error || String(e)
  } finally {
    proxySaving.value = false
  }
}

async function handleProxyClear() {
  proxyError.value = ""
  keysError.value = ""
  proxySaving.value = true
  try {
    await store.saveSettings({ proxy_url: null })
    proxyUrl.value = ""
    toast.success("Proxy settings cleared")
  } catch (e: any) {
    proxyError.value = store.error || String(e)
  } finally {
    proxySaving.value = false
  }
}

function addApiKey() {
  apiKeys.value = [...apiKeys.value, ""]
  revealKeys.value = { ...revealKeys.value, [apiKeys.value.length - 1]: true }
}

function removeApiKey(idx: number) {
  apiKeys.value = apiKeys.value.filter((_, i) => i !== idx)
  const next: Record<number, boolean> = {}
  apiKeys.value.forEach((_, i) => {
    if (revealKeys.value[i + (i >= idx ? 1 : 0)]) next[i] = true
  })
  revealKeys.value = next
}

function updateApiKey(idx: number, value: string) {
  apiKeys.value = apiKeys.value.map((k, i) => (i === idx ? value : k))
}

function toggleReveal(idx: number) {
  revealKeys.value = { ...revealKeys.value, [idx]: !revealKeys.value[idx] }
}

async function handleKeysSave() {
  keysError.value = ""
  proxyError.value = ""
  const trimmed = apiKeys.value.map((k) => k.trim()).filter((k) => k.length > 0)
  // Detect blank entries that were intentionally added (non-empty raw form
  // is not needed — any all-whitespace row after trim is an error if the
  // user still has rows in the form).
  if (apiKeys.value.some((k) => k.length > 0 && k.trim().length === 0)) {
    keysError.value = "API keys must not be empty or whitespace-only."
    return
  }
  if (apiKeys.value.some((k) => k.length === 0)) {
    keysError.value = "Remove empty key rows before saving, or fill them in."
    return
  }
  keysSaving.value = true
  try {
    // Keep the current proxy so a keys-only save never clears it.
    await store.saveSettings({
      proxy_url: store.proxyUrl,
      api_keys: trimmed,
    })
    apiKeys.value = [...store.apiKeys]
    revealKeys.value = {}
    if (trimmed.length === 0) {
      toast.success("Authentication disabled")
    } else {
      toast.success("API keys saved")
    }
  } catch (e: any) {
    keysError.value = store.error || String(e)
  } finally {
    keysSaving.value = false
  }
}

async function handleKeysDisable() {
  keysError.value = ""
  proxyError.value = ""
  keysSaving.value = true
  try {
    await store.saveSettings({
      proxy_url: store.proxyUrl,
      api_keys: [],
    })
    apiKeys.value = []
    revealKeys.value = {}
    toast.success("Authentication disabled")
  } catch (e: any) {
    keysError.value = store.error || String(e)
  } finally {
    keysSaving.value = false
  }
}

/** Mask a key for the status badge (never log or toast the full value). */
function keyFingerprint(key: string): string {
  if (!key) return "empty"
  if (key.length <= 8) return "••••"
  return `${key.slice(0, 3)}…${key.slice(-4)}`
}
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0 space-y-4">
      <div class="flex flex-wrap items-center justify-between gap-3">
        <div class="min-w-0">
          <h1 class="text-lg font-semibold xl:text-xl">Settings</h1>
          <p class="text-muted-foreground text-sm">
            Gateway-level configuration for proxy and client authentication.
          </p>
        </div>
        <RefreshButton :loading="store.loading" @click="refreshPage" />
      </div>
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto pt-4 pb-4">
      <div class="grid grid-cols-1 gap-4 xl:grid-cols-2">
        <!-- ── Global Proxy ─────────────────────────────────────────── -->
        <Card class="flex flex-col">
          <CardHeader>
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0 space-y-1.5">
                <CardTitle class="flex items-center gap-2">
                  <Globe class="h-4 w-4" />
                  Global Proxy URL
                </CardTitle>
                <CardDescription>
                  Upstream HTTP/HTTPS proxy for providers whose proxy mode is
                  <code>inherit</code> or <code>enabled</code>. SOCKS is not supported.
                </CardDescription>
              </div>
              <Badge variant="outline" class="shrink-0 text-[10px]">
                {{ store.proxyUrl ? "Configured" : "Direct" }}
              </Badge>
            </div>
          </CardHeader>
          <CardContent class="flex flex-1 flex-col gap-3">
            <div v-if="store.loading" class="text-muted-foreground flex items-center gap-2 text-xs">
              <Loader2 class="h-3.5 w-3.5 animate-spin" />
              Loading settings...
            </div>

            <Input
              v-model="proxyUrl"
              :disabled="store.loading || proxySaving"
              placeholder="http://proxy.example:8080"
              class="font-mono"
              @keydown.enter.prevent="handleProxySave"
            />

            <div
              v-if="proxyError"
              class="bg-destructive/10 text-destructive flex items-start gap-2 rounded-md px-3 py-2 text-xs"
            >
              <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>{{ proxyError }}</span>
            </div>

            <div class="mt-auto flex items-center gap-2">
              <Button :disabled="store.loading || proxySaving" @click="handleProxySave">
                <Loader2 v-if="proxySaving" class="h-3.5 w-3.5 animate-spin" />
                <Save v-else class="h-3.5 w-3.5" />
                Save
              </Button>
              <Button
                variant="outline"
                :disabled="store.loading || proxySaving || !store.proxyUrl"
                @click="handleProxyClear"
              >
                <Trash2 class="h-3.5 w-3.5" />
                Clear
              </Button>
            </div>
          </CardContent>
        </Card>

        <!-- ── Gateway API Keys ─────────────────────────────────────── -->
        <Card class="flex flex-col">
          <CardHeader>
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0 space-y-1.5">
                <CardTitle class="flex items-center gap-2">
                  <KeyRound class="h-4 w-4" />
                  Gateway API Keys
                </CardTitle>
                <CardDescription>
                  Clients must send <code>Authorization: Bearer &lt;key&gt;</code>. An empty list
                  disables authentication.
                </CardDescription>
              </div>
              <Badge
                variant="secondary"
                class="shrink-0 gap-1 text-[10px]"
                :class="store.apiKeys.length > 0 ? '' : 'text-muted-foreground'"
              >
                <component
                  :is="store.apiKeys.length > 0 ? ShieldCheck : ShieldOff"
                  class="h-3 w-3"
                />
                {{ store.apiKeys.length > 0 ? "Enabled" : "Disabled" }}
              </Badge>
            </div>
          </CardHeader>
          <CardContent class="flex flex-1 flex-col gap-3">
            <div v-if="store.loading" class="text-muted-foreground flex items-center gap-2 text-xs">
              <Loader2 class="h-3.5 w-3.5 animate-spin" />
              Loading settings...
            </div>

            <div v-else-if="apiKeys.length === 0" class="text-muted-foreground text-sm">
              Authentication is disabled. Add a key to require clients to authenticate against this
              gateway.
            </div>

            <div v-else class="space-y-2">
              <div v-for="(key, idx) in apiKeys" :key="idx" class="flex items-center gap-2">
                <div class="relative min-w-0 flex-1">
                  <Input
                    :type="revealKeys[idx] ? 'text' : 'password'"
                    :model-value="key"
                    :placeholder="`sk-... (key ${idx + 1})`"
                    class="pr-9 font-mono"
                    :disabled="keysSaving"
                    @input="updateApiKey(idx, ($event.target as HTMLInputElement).value)"
                  />
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    class="text-muted-foreground hover:text-foreground absolute top-1/2 right-1 -translate-y-1/2"
                    :disabled="keysSaving"
                    @click="toggleReveal(idx)"
                  >
                    <component :is="revealKeys[idx] ? EyeOff : Eye" class="h-3.5 w-3.5" />
                  </Button>
                </div>
                <Badge variant="outline" class="hidden shrink-0 font-mono text-[10px] sm:inline">
                  {{ keyFingerprint(key) }}
                </Badge>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  :disabled="keysSaving"
                  @click="removeApiKey(idx)"
                >
                  <X class="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>

            <div
              v-if="keysError"
              class="bg-destructive/10 text-destructive flex items-start gap-2 rounded-md px-3 py-2 text-xs"
            >
              <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>{{ keysError }}</span>
            </div>

            <div v-if="store.apiKeys.length > 0" class="text-muted-foreground text-xs">
              Removing the key your dashboard is using will lock you out until you reconnect with a
              remaining key.
            </div>

            <div class="mt-auto flex flex-wrap items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                :disabled="store.loading || keysSaving"
                @click="addApiKey"
              >
                <Plus class="h-3.5 w-3.5" />
                Add Key
              </Button>
              <Button :disabled="store.loading || keysSaving" @click="handleKeysSave">
                <Loader2 v-if="keysSaving" class="h-3.5 w-3.5 animate-spin" />
                <Save v-else class="h-3.5 w-3.5" />
                Save Keys
              </Button>
              <Button
                v-if="store.apiKeys.length > 0"
                variant="outline"
                :disabled="store.loading || keysSaving"
                @click="handleKeysDisable"
              >
                <ShieldOff class="h-3.5 w-3.5" />
                Disable Auth
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    </div>
  </div>
</template>

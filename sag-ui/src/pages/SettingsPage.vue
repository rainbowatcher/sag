<script setup lang="ts">
import {
  AlertCircle,
  Clock,
  Database,
  Eye,
  EyeOff,
  Globe,
  KeyRound,
  Languages,
  Loader2,
  Plus,
  Save,
  ScrollText,
  Server,
  ShieldCheck,
  ShieldOff,
  Timer,
  Trash2,
  X,
  Zap,
} from "@lucide/vue"
import { computed, onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import RefreshButton from "@/components/RefreshButton.vue"
import BackoffForm from "@/components/settings/BackoffForm.vue"
import CircuitBreakerForm from "@/components/settings/CircuitBreakerForm.vue"
import StrategyTimersForm from "@/components/settings/StrategyTimersForm.vue"
import SystemInfoCard, { type SystemInfoRow } from "@/components/settings/SystemInfoCard.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { useI18n, type Locale } from "@/lib/i18n"
import { useGatewayConfigStore } from "@/stores/gatewayConfig"
import { useSettingsStore } from "@/stores/settings"

const store = useSettingsStore()
const gatewayConfig = useGatewayConfigStore()

// UI language is the only string set migrated in P0; every other label on this
// page stays hardcoded until the per-module migration phases land.
const { t, locale, setLocale } = useI18n()

const activeTab = ref("network")

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
    return t(
      "settings.proxy.invalidUrl",
      undefined,
      "Invalid URL. Use the form http://host:port or https://host:port.",
    )
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return t(
      "settings.proxy.httpOnly",
      undefined,
      "Only HTTP/HTTPS proxy URLs are supported (SOCKS is not supported).",
    )
  }
  return null
}

function syncFormsFromStore() {
  proxyUrl.value = store.proxyUrl ?? ""
  apiKeys.value = [...store.apiKeys]
  revealKeys.value = {}
}

// ── Read-only config sections ─────────────────────────────────────
// `server` / `logging` / `analytics` / `proxy_url` are not writable through
// `PUT /api/config`; they are rendered as read-only rows, and a section
// missing from the snapshot falls back to a muted placeholder.
const serverRows = computed<SystemInfoRow[]>(() => {
  const server = gatewayConfig.config?.server
  const proxyUrl = gatewayConfig.config?.proxy_url
  return [
    { label: t("settings.info.host", undefined, "Host"), value: server?.host, mono: true },
    { label: t("settings.info.port", undefined, "Port"), value: server?.port, mono: true },
    // `null` (not `undefined`) means the snapshot loaded and no proxy is set.
    {
      label: t("settings.info.globalProxy", undefined, "Global proxy"),
      value:
        proxyUrl === null ? t("settings.info.direct", undefined, "Direct (no proxy)") : proxyUrl,
      mono: true,
    },
  ]
})

const loggingRows = computed<SystemInfoRow[]>(() => {
  const logging = gatewayConfig.config?.logging
  return [
    { label: t("settings.info.level", undefined, "Level"), value: logging?.level, mono: true },
    { label: t("settings.info.format", undefined, "Format"), value: logging?.format, mono: true },
  ]
})

const analyticsRows = computed<SystemInfoRow[]>(() => {
  const analytics = gatewayConfig.config?.analytics
  if (!analytics) {
    return [
      { label: t("settings.info.enabled", undefined, "Enabled"), value: null },
      { label: t("settings.info.storageDsn", undefined, "Storage DSN"), value: null },
    ]
  }
  return [
    {
      label: t("settings.info.enabled", undefined, "Enabled"),
      value: analytics.enabled ? t("common.yes") : t("common.no"),
    },
    {
      label: t("settings.info.storageDsn", undefined, "Storage DSN"),
      value: analytics.storage_dsn,
      mono: true,
    },
  ]
})

async function refreshPage() {
  proxyError.value = ""
  keysError.value = ""
  await Promise.all([store.fetchSettings(), gatewayConfig.fetchConfig()])
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
    toast.success(t("settings.toast.proxySaved", undefined, "Proxy settings saved"))
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
    toast.success(t("settings.toast.proxyCleared", undefined, "Proxy settings cleared"))
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
  if (apiKeys.value.some((k) => k.length > 0 && k.trim().length === 0)) {
    keysError.value = t(
      "settings.apiKeys.emptyOrWhitespace",
      undefined,
      "API keys must not be empty or whitespace-only.",
    )
    return
  }
  if (apiKeys.value.some((k) => k.length === 0)) {
    keysError.value = t(
      "settings.apiKeys.emptyRows",
      undefined,
      "Remove empty key rows before saving, or fill them in.",
    )
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
      toast.success(t("settings.toast.authDisabled", undefined, "Authentication disabled"))
    } else {
      toast.success(t("settings.toast.apiKeysSaved", undefined, "API keys saved"))
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
    toast.success(t("settings.toast.authDisabled", undefined, "Authentication disabled"))
  } catch (e: any) {
    keysError.value = store.error || String(e)
  } finally {
    keysSaving.value = false
  }
}

/** Mask a key for the status badge (never log or toast the full value). */
function keyFingerprint(key: string): string {
  if (!key) return t("settings.auth.keyEmpty")
  if (key.length <= 8) return "••••"
  return `${key.slice(0, 3)}…${key.slice(-4)}`
}
</script>

<template>
  <div class="flex h-full flex-col">
    <div class="shrink-0">
      <div class="flex flex-wrap items-center justify-between gap-3">
        <div class="min-w-0">
          <h1 class="text-lg font-semibold xl:text-xl">{{ $t("nav.settings") }}</h1>
          <p class="text-muted-foreground text-sm">
            {{ $t("settings.page.description") }}
          </p>
        </div>
        <RefreshButton :loading="store.loading" @click="refreshPage" />
      </div>
    </div>

    <Tabs v-model="activeTab" class="mt-4 flex min-h-0 flex-1 flex-col">
      <TabsList class="shrink-0 flex-wrap self-start">
        <TabsTrigger value="network" class="gap-1.5">
          <Globe class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.network") }}
        </TabsTrigger>
        <TabsTrigger value="auth" class="gap-1.5">
          <KeyRound class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.authentication") }}
          <Badge
            v-if="!store.loading"
            variant="secondary"
            class="ml-1 h-4 px-1 text-[10px]"
            :class="store.apiKeys.length > 0 ? '' : 'text-muted-foreground'"
          >
            {{ store.apiKeys.length > 0 ? $t("common.on") : $t("common.off") }}
          </Badge>
        </TabsTrigger>
        <TabsTrigger value="backoff" class="gap-1.5">
          <Timer class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.backoff") }}
        </TabsTrigger>
        <TabsTrigger value="breaker" class="gap-1.5">
          <Zap class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.breaker") }}
        </TabsTrigger>
        <TabsTrigger value="timers" class="gap-1.5">
          <Clock class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.timers") }}
        </TabsTrigger>
        <TabsTrigger value="analytics" class="gap-1.5">
          <Database class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.analytics") }}
        </TabsTrigger>
        <TabsTrigger value="system" class="gap-1.5">
          <Server class="h-3.5 w-3.5" />
          {{ $t("settings.tabs.system") }}
        </TabsTrigger>
        <TabsTrigger value="language" class="gap-1.5">
          <Languages class="h-3.5 w-3.5" />
          {{ t("settings.language.tab") }}
        </TabsTrigger>
      </TabsList>

      <div class="min-h-0 flex-1 overflow-y-auto pt-4 pb-4">
        <!-- ── Network / Proxy ──────────────────────────────────────── -->
        <TabsContent value="network" class="mt-0">
          <Card>
            <CardHeader>
              <div class="flex flex-wrap items-start justify-between gap-3">
                <div class="max-w-3xl min-w-0 space-y-1.5">
                  <CardTitle class="flex items-center gap-2">
                    <Globe class="h-4 w-4" />
                    {{ $t("settings.proxy.title") }}
                  </CardTitle>
                  <CardDescription>
                    {{ $t("settings.proxy.descriptionLead") }}<code>inherit</code
                    >{{ $t("settings.proxy.descriptionMid") }}<code>enabled</code
                    >{{ $t("settings.proxy.descriptionTail") }}
                  </CardDescription>
                </div>
                <Badge variant="outline" class="shrink-0 text-[10px]">
                  {{
                    store.proxyUrl ? $t("settings.proxy.configured") : $t("settings.proxy.direct")
                  }}
                </Badge>
              </div>
            </CardHeader>
            <CardContent class="space-y-4">
              <div
                v-if="store.loading"
                class="text-muted-foreground flex items-center gap-2 text-xs"
              >
                <Loader2 class="h-3.5 w-3.5 animate-spin" />
                {{ $t("settings.loadingSettings") }}
              </div>

              <div class="max-w-2xl space-y-2 xl:max-w-3xl">
                <Input
                  v-model="proxyUrl"
                  :disabled="store.loading || proxySaving"
                  placeholder="http://proxy.example:8080"
                  class="font-mono"
                  @keydown.enter.prevent="handleProxySave"
                />
                <p class="text-muted-foreground text-xs">
                  {{ $t("settings.proxy.hint") }}
                </p>
              </div>

              <div
                v-if="proxyError"
                class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
              >
                <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>{{ proxyError }}</span>
              </div>

              <div class="flex flex-wrap items-center gap-2 border-t pt-4">
                <Button :disabled="store.loading || proxySaving" @click="handleProxySave">
                  <Loader2 v-if="proxySaving" class="h-3.5 w-3.5 animate-spin" />
                  <Save v-else class="h-3.5 w-3.5" />
                  {{ $t("common.save") }}
                </Button>
                <Button
                  variant="outline"
                  :disabled="store.loading || proxySaving || !store.proxyUrl"
                  @click="handleProxyClear"
                >
                  <Trash2 class="h-3.5 w-3.5" />
                  {{ $t("common.clear") }}
                </Button>
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        <!-- ── Authentication / API Keys ────────────────────────────── -->
        <TabsContent value="auth" class="mt-0">
          <Card>
            <CardHeader>
              <div class="flex flex-wrap items-start justify-between gap-3">
                <div class="max-w-3xl min-w-0 space-y-1.5">
                  <CardTitle class="flex items-center gap-2">
                    <KeyRound class="h-4 w-4" />
                    {{ $t("settings.auth.title") }}
                  </CardTitle>
                  <CardDescription>
                    {{ $t("settings.auth.descriptionLead")
                    }}<code>Authorization: Bearer &lt;key&gt;</code
                    >{{ $t("settings.auth.descriptionTail") }}
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
                  {{ store.apiKeys.length > 0 ? $t("common.enabled") : $t("common.disabled") }}
                </Badge>
              </div>
            </CardHeader>
            <CardContent class="space-y-4">
              <div
                v-if="store.loading"
                class="text-muted-foreground flex items-center gap-2 text-xs"
              >
                <Loader2 class="h-3.5 w-3.5 animate-spin" />
                {{ $t("settings.loadingSettings") }}
              </div>

              <template v-else>
                <div v-if="apiKeys.length === 0" class="text-muted-foreground max-w-3xl text-sm">
                  {{ $t("settings.auth.disabledHint") }}
                </div>

                <div v-else class="space-y-2">
                  <div
                    v-for="(key, idx) in apiKeys"
                    :key="idx"
                    class="flex items-center gap-2 xl:max-w-4xl"
                  >
                    <span class="text-muted-foreground w-8 shrink-0 text-right font-mono text-xs">
                      {{ idx + 1 }}.
                    </span>
                    <div class="relative min-w-0 flex-1">
                      <Input
                        :type="revealKeys[idx] ? 'text' : 'password'"
                        :model-value="key"
                        :placeholder="$t('settings.auth.keyPlaceholder', { index: idx + 1 })"
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
                    <Badge
                      variant="outline"
                      class="hidden shrink-0 font-mono text-[10px] sm:inline"
                    >
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
                  class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
                >
                  <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  <span>{{ keysError }}</span>
                </div>

                <div
                  v-if="store.apiKeys.length > 0"
                  class="text-muted-foreground max-w-3xl text-xs"
                >
                  {{ $t("settings.auth.lockoutWarning") }}
                </div>
              </template>

              <div class="flex flex-wrap items-center gap-2 border-t pt-4">
                <Button
                  variant="outline"
                  size="sm"
                  :disabled="store.loading || keysSaving"
                  @click="addApiKey"
                >
                  <Plus class="h-3.5 w-3.5" />
                  {{ $t("settings.auth.addKey") }}
                </Button>
                <Button :disabled="store.loading || keysSaving" @click="handleKeysSave">
                  <Loader2 v-if="keysSaving" class="h-3.5 w-3.5 animate-spin" />
                  <Save v-else class="h-3.5 w-3.5" />
                  {{ $t("settings.auth.saveKeys") }}
                </Button>
                <Button
                  v-if="store.apiKeys.length > 0"
                  variant="outline"
                  :disabled="store.loading || keysSaving"
                  @click="handleKeysDisable"
                >
                  <ShieldOff class="h-3.5 w-3.5" />
                  {{ $t("settings.auth.disableAuth") }}
                </Button>
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        <!-- ── Backoff ──────────────────────────────────────────────── -->
        <TabsContent value="backoff" class="mt-0">
          <div
            v-if="gatewayConfig.loading"
            class="text-muted-foreground flex items-center gap-2 text-xs"
          >
            <Loader2 class="h-3.5 w-3.5 animate-spin" />
            {{ $t("settings.loadingConfig") }}
          </div>
          <div
            v-else-if="gatewayConfig.error && !gatewayConfig.config"
            class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
          >
            <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{{ gatewayConfig.error }}</span>
          </div>
          <BackoffForm v-else />
        </TabsContent>

        <!-- ── Circuit breaker ──────────────────────────────────────── -->
        <TabsContent value="breaker" class="mt-0">
          <div
            v-if="gatewayConfig.loading"
            class="text-muted-foreground flex items-center gap-2 text-xs"
          >
            <Loader2 class="h-3.5 w-3.5 animate-spin" />
            {{ $t("settings.loadingConfig") }}
          </div>
          <div
            v-else-if="gatewayConfig.error && !gatewayConfig.config"
            class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
          >
            <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{{ gatewayConfig.error }}</span>
          </div>
          <CircuitBreakerForm v-else />
        </TabsContent>

        <!-- ── Strategy timers ──────────────────────────────────────── -->
        <TabsContent value="timers" class="mt-0">
          <div
            v-if="gatewayConfig.loading"
            class="text-muted-foreground flex items-center gap-2 text-xs"
          >
            <Loader2 class="h-3.5 w-3.5 animate-spin" />
            {{ $t("settings.loadingConfig") }}
          </div>
          <div
            v-else-if="gatewayConfig.error && !gatewayConfig.config"
            class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
          >
            <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{{ gatewayConfig.error }}</span>
          </div>
          <StrategyTimersForm v-else />
        </TabsContent>

        <!-- ── Analytics (read-only) ────────────────────────────────── -->
        <TabsContent value="analytics" class="mt-0">
          <div
            v-if="gatewayConfig.loading"
            class="text-muted-foreground flex items-center gap-2 text-xs"
          >
            <Loader2 class="h-3.5 w-3.5 animate-spin" />
            {{ $t("settings.loadingConfig") }}
          </div>
          <SystemInfoCard
            v-else
            :title="$t('settings.tabs.analytics')"
            :description="$t('settings.analytics.description')"
            :badge="$t('settings.info.requiresRestart')"
            :icon="Database"
            :rows="analyticsRows"
          />
        </TabsContent>

        <!-- ── System (read-only) ───────────────────────────────────── -->
        <TabsContent value="system" class="mt-0 space-y-4">
          <div
            v-if="gatewayConfig.loading"
            class="text-muted-foreground flex items-center gap-2 text-xs"
          >
            <Loader2 class="h-3.5 w-3.5 animate-spin" />
            {{ $t("settings.loadingConfig") }}
          </div>
          <template v-else>
            <SystemInfoCard
              :title="$t('settings.system.serverTitle')"
              :description="$t('settings.system.serverDescription')"
              :badge="$t('settings.info.requiresRestart')"
              :icon="Server"
              :rows="serverRows"
            />
            <SystemInfoCard
              :title="$t('settings.system.loggingTitle')"
              :description="$t('settings.system.loggingDescription')"
              :badge="$t('settings.info.requiresRestart')"
              :icon="ScrollText"
              :rows="loggingRows"
            />
            <p class="text-muted-foreground max-w-3xl text-xs">
              {{ $t("settings.system.restartNoteLead") }}<code>gateway.toml</code
              >{{ $t("settings.system.restartNoteTail") }}
            </p>
          </template>
        </TabsContent>

        <!-- ── Language (P0 i18n switch) ──────────────────────────── -->
        <TabsContent value="language" class="mt-0">
          <Card>
            <CardHeader>
              <div class="max-w-3xl min-w-0 space-y-1.5">
                <CardTitle class="flex items-center gap-2">
                  <Languages class="h-4 w-4" />
                  {{ t("settings.language.title") }}
                </CardTitle>
                <CardDescription>
                  {{ t("settings.language.description") }}
                </CardDescription>
              </div>
            </CardHeader>
            <CardContent class="max-w-2xl space-y-2 xl:max-w-3xl">
              <FieldLabel for="ui-language">
                {{ t("settings.language.label") }}
              </FieldLabel>
              <Select :model-value="locale" @update:model-value="(v) => setLocale(v as Locale)">
                <SelectTrigger id="ui-language" class="w-48">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="en">{{ t("settings.language.optionEnglish") }}</SelectItem>
                  <SelectItem value="zh">{{ t("settings.language.optionChinese") }}</SelectItem>
                </SelectContent>
              </Select>
            </CardContent>
          </Card>
        </TabsContent>
      </div>
    </Tabs>
  </div>
</template>

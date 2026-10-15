<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { ref } from "vue"
import { toast } from "vue-sonner"
import ApisEditor from "@/components/providers/ApisEditor.vue"
import ProviderAdvancedTab from "@/components/providers/ProviderAdvancedTab.vue"
import ProviderBasicFields from "@/components/providers/ProviderBasicFields.vue"
import ProviderModelsTab from "@/components/providers/ProviderModelsTab.vue"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { useProviderProbe } from "@/composables/useProviderProbe"
import { useResponsiveModal } from "@/composables/useResponsiveModal"
import { t } from "@/lib/i18n"
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import type { Provider, ProxyMode } from "@/types/api"

const { Modal } = useResponsiveModal()

const store = useProviderStore()

const props = defineProps<{
  provider: Provider
}>()

const emit = defineEmits<{
  close: []
  updated: []
}>()

const open = ref(true)
const saving = ref(false)
const apiError = ref("")

function onOpenChange(v: boolean) {
  if (!v) emit("close")
}

// ── Basic fields ─────────────────────────────────────────────
const timeoutSecs = ref(props.provider.timeout_secs)

// ── Proxy mode ───────────────────────────────────────────────
// Initialize from the saved provider; the current in-memory selection is
// always what gets sent (explicitly, so switching back to inherit works).
const proxyMode = ref<ProxyMode>(props.provider.proxy_mode ?? "inherit")

// ── API keys ─────────────────────────────────────────────────
const apiKeys = ref<string[]>([...props.provider.api_keys])
const keyStrategy = ref<"fill_first" | "round_robin">(props.provider.key_strategy)

// ── Models ───────────────────────────────────────────────────
const enabledModels = ref<string[]>([...props.provider.enabled_models])

// ── KV pairs ─────────────────────────────────────────────────
const headersList = ref<{ key: string; value: string }[]>(
  Object.entries(props.provider.headers ?? {}).map(([k, v]) => ({ key: k, value: v })),
)
const bodyOverridesList = ref<{ key: string; value: string }[]>(
  Object.entries(props.provider.body_overrides ?? {}).map(([k, v]) => ({
    key: k,
    value: typeof v === "string" ? v : JSON.stringify(v),
  })),
)
const stripFieldsList = ref<string[]>([...(props.provider.strip_fields ?? [])])
const roleMappingsList = ref<{ key: string; value: string }[]>(
  Object.entries(props.provider.role_mappings ?? {}).map(([k, v]) => ({ key: k, value: v })),
)

function headersMap(): Record<string, string> {
  const m: Record<string, string> = {}
  for (const { key, value } of headersList.value) {
    if (key.trim() && value.trim()) m[key.trim()] = value.trim()
  }
  return m
}

function roleMappingsMap(): Record<string, string> {
  const m: Record<string, string> = {}
  for (const { key, value } of roleMappingsList.value) {
    if (key.trim() && value.trim()) m[key.trim()] = value.trim()
  }
  return m
}

// ── Protocol probe (shared composable) ───────────────────────
const {
  apis,
  fallback,
  apisEditor,
  protocolSections,
  probeKind,
  probeApi,
  onFetchModels,
  onTestModel,
} = useProviderProbe({
  initialApis: props.provider.apis,
  initialFallback: props.provider.fallback ?? null,
  apiKeys,
  proxyMode,
  getHeaders: headersMap,
})

// ── Submit ───────────────────────────────────────────────────
async function handleSubmit() {
  apiError.value = ""
  saving.value = true
  try {
    const validKeys = apiKeys.value.filter((k) => k.trim())
    const payload: Record<string, any> = {}

    const apisError = apisEditor.value?.validate()
    if (apisError) {
      apiError.value = apisError
      return
    }

    if (validKeys.length > 0) {
      payload.api_keys = validKeys
    }
    payload.key_strategy = keyStrategy.value

    // Whole-directory replace: send the full catalog on every save.
    payload.apis = apis.value
    // Triple-state fallback patch: a value sets, `null` clears (back to the
    // single-protocol auto default) — both states are always explicit here.
    payload.fallback = fallback.value

    if (timeoutSecs.value !== props.provider.timeout_secs) {
      payload.timeout_secs = timeoutSecs.value
    }

    const parsedHeaders = headersMap()
    if (JSON.stringify(parsedHeaders) !== JSON.stringify(props.provider.headers ?? {})) {
      payload.headers = parsedHeaders
    }

    const parsedRoleMappings = roleMappingsMap()
    if (JSON.stringify(parsedRoleMappings) !== JSON.stringify(props.provider.role_mappings ?? {})) {
      payload.role_mappings = parsedRoleMappings
    }

    const parsedBodyOverrides: Record<string, any> = {}
    for (const { key, value: val } of bodyOverridesList.value) {
      if (!key.trim()) continue
      try {
        parsedBodyOverrides[key.trim()] = val.trim() ? JSON.parse(val) : val
      } catch {
        apiError.value = t("providers.form.bodyOverrideInvalidJson", { key })
        return
      }
    }
    if (
      JSON.stringify(parsedBodyOverrides) !== JSON.stringify(props.provider.body_overrides ?? {})
    ) {
      payload.body_overrides = parsedBodyOverrides
    }

    const parsedStripFields = stripFieldsList.value.filter((s) => s.trim()).map((s) => s.trim())
    if (JSON.stringify(parsedStripFields) !== JSON.stringify(props.provider.strip_fields ?? [])) {
      payload.strip_fields = parsedStripFields
    }

    if (
      JSON.stringify(enabledModels.value) !== JSON.stringify(props.provider.enabled_models ?? [])
    ) {
      payload.enabled_models = enabledModels.value
    }

    // Always send the current proxy mode — an explicit "inherit" is required
    // to reset a provider that was saved as enabled/disabled.
    payload.proxy_mode = proxyMode.value

    await store.updateProvider(props.provider.name, payload)
    toast.success(t("providers.toast.updated", { name: props.provider.name }))
    emit("updated")
  } catch (e: any) {
    if (apiError.value) return
    const msg = getErrorMessage(e)
    apiError.value = msg
    toast.error(msg)
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <component :is="Modal.Root" :open="open" @update:open="onOpenChange">
    <component :is="Modal.Content" class="flex max-h-[85vh] flex-col overflow-hidden sm:max-w-2xl">
      <component :is="Modal.Header" class="shrink-0">
        <component :is="Modal.Title">
          {{ provider.name }}
        </component>
        <component :is="Modal.Description">
          {{ $t("providers.editForm.description") }}
        </component>
      </component>

      <div
        v-if="apiError"
        class="mx-6 mb-2 shrink-0 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500"
      >
        {{ apiError }}
      </div>

      <div class="min-w-0 flex-1 overflow-y-auto px-2 pb-4 sm:px-6">
        <Tabs default-value="basic">
          <TabsList class="mb-4">
            <TabsTrigger value="basic">{{ $t("providers.tabs.basic") }}</TabsTrigger>
            <TabsTrigger value="apis">{{ $t("providers.tabs.apis") }}</TabsTrigger>
            <TabsTrigger value="models">{{ $t("providers.tabs.models") }}</TabsTrigger>
            <TabsTrigger value="advanced">{{ $t("providers.tabs.advanced") }}</TabsTrigger>
          </TabsList>

          <!-- ─── Tab: Basic ──────────────────────────────────── -->
          <TabsContent value="basic" class="grid grid-cols-2 gap-4">
            <div>
              <FieldLabel>{{ $t("common.name") }}</FieldLabel>
              <Input :model-value="provider.name" disabled class="bg-muted text-muted-foreground" />
            </div>

            <div class="space-y-1">
              <FieldLabel>{{ $t("providers.form.timeoutLabel") }}</FieldLabel>
              <Input
                type="number"
                :model-value="timeoutSecs"
                @input="timeoutSecs = Number(($event.target as HTMLInputElement).value)"
              />
            </div>

            <ProviderBasicFields
              v-model:api-keys="apiKeys"
              v-model:key-strategy="keyStrategy"
              v-model:proxy-mode="proxyMode"
            />
          </TabsContent>

          <!-- ─── Tab: APIs ───────────────────────────────────── -->
          <TabsContent value="apis" class="min-w-0 space-y-4">
            <ApisEditor
              ref="apisEditor"
              v-model="apis"
              v-model:fallback="fallback"
              :api-keys="apiKeys"
              :proxy-mode="proxyMode"
              :provider-headers="headersMap()"
            />
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <ProviderModelsTab
              :provider-name="provider.name"
              :protocol-sections="protocolSections"
              :probe-api="probeApi"
              v-model:probe-kind="probeKind"
              v-model:enabled-models="enabledModels"
              @fetch-models="onFetchModels"
              @test-model="onTestModel"
            />
          </TabsContent>

          <!-- ─── Tab: Advanced ───────────────────────────────── -->
          <TabsContent value="advanced" class="space-y-4">
            <ProviderAdvancedTab
              v-model:headers="headersList"
              v-model:body-overrides="bodyOverridesList"
              v-model:strip-fields="stripFieldsList"
              v-model:role-mappings="roleMappingsList"
              show-body-override-warning
              :role-mappings-description="$t('providers.advanced.roleMappingsDescriptionEdit')"
            />
          </TabsContent>
        </Tabs>
      </div>

      <component :is="Modal.Footer" class="shrink-0">
        <component :is="Modal.Close" as-child>
          <Button variant="outline" size="sm" :disabled="saving">
            {{ $t("common.cancel") }}
          </Button>
        </component>
        <Button size="sm" :disabled="saving" @click="handleSubmit">
          <Loader2 v-if="saving" class="h-3 w-3 animate-spin" />
          {{ saving ? $t("common.saving") : $t("common.save") }}
        </Button>
      </component>
    </component>
  </component>
</template>

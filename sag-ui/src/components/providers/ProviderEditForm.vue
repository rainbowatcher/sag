<script setup lang="ts">
import { Loader2, X } from "@lucide/vue"
import { useMediaQuery } from "@vueuse/core"
import { computed, onMounted, ref, watch } from "vue"
import { toast } from "vue-sonner"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import KvEditor from "@/components/KvEditor.vue"
import ModelManager from "@/components/ModelManager.vue"
import ApisEditor from "@/components/providers/ApisEditor.vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  Drawer,
  DrawerClose,
  DrawerContent,
  DrawerDescription,
  DrawerFooter,
  DrawerHeader,
  DrawerTitle,
} from "@/components/ui/drawer"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { API_KIND_LABELS, PROBE_KIND_ORDER, defaultProbeKind } from "@/lib/provider-probe"
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import type { ApiConfig, ApiKind, Provider, ProxyMode } from "@/types/api"

const isDesktop = useMediaQuery("(min-width: 640px)")

const Modal = computed(() => ({
  Root: isDesktop.value ? Dialog : Drawer,
  Trigger: isDesktop.value ? "div" : "div",
  Content: isDesktop.value ? DialogContent : DrawerContent,
  Header: isDesktop.value ? DialogHeader : DrawerHeader,
  Title: isDesktop.value ? DialogTitle : DrawerTitle,
  Description: isDesktop.value ? DialogDescription : DrawerDescription,
  Footer: isDesktop.value ? DialogFooter : DrawerFooter,
  Close: isDesktop.value ? DialogClose : DrawerClose,
}))

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

onMounted(() => {
  store.upstreamModels = []
  store.upstreamModelsError = ""
})

function onOpenChange(v: boolean) {
  if (!v) emit("close")
}

// ── Basic fields ─────────────────────────────────────────────
const timeoutSecs = ref(props.provider.timeout_secs)

// ── Proxy mode ───────────────────────────────────────────────
// Initialize from the saved provider; the current in-memory selection is
// always what gets sent (explicitly, so switching back to inherit works).
const proxyMode = ref<ProxyMode>(props.provider.proxy_mode ?? "inherit")

// ── API sections (protocol catalog) ──────────────────────────
// The editor owns the whole directory: updates always send the full
// `apis` map (whole-directory replace on the backend).
const apis = ref<Partial<Record<ApiKind, ApiConfig>>>({ ...props.provider.apis })
const fallback = ref<ApiKind | null>(props.provider.fallback ?? null)
const apisEditor = ref<{ validate: () => string | null } | null>(null)

/**
 * Default section for model listing / model tests: the `fallback` protocol
 * when declared, else chat_completions, else the next protocol key — never
 * the `image` section, independent of key insertion order.
 */
const primaryKind = computed<ApiKind | null>(() => defaultProbeKind(apis.value, fallback.value))

/** Sections selectable as the models-probe target (protocol keys only, fixed order). */
const protocolSections = computed<ApiKind[]>(() => {
  const keys = Object.keys(apis.value) as ApiKind[]
  return PROBE_KIND_ORDER.filter((k) => keys.includes(k))
})

/** Currently selected probe section; falls back to the default when it disappears. */
const probeKind = ref<ApiKind | null>(null)
watch(
  [primaryKind, protocolSections],
  () => {
    if (!probeKind.value || !protocolSections.value.includes(probeKind.value)) {
      probeKind.value = primaryKind.value
    }
  },
  { immediate: true },
)
const probeApi = computed<ApiConfig | null>(() =>
  probeKind.value ? (apis.value[probeKind.value] ?? null) : null,
)

/** Translated-lane protocol badge shown next to models. */
const defaultProtocol = computed<ApiKind | undefined>(() => {
  const protocols = (Object.keys(apis.value) as ApiKind[]).filter((k) => k !== "image")
  if (fallback.value) return fallback.value
  return protocols.length === 1 ? protocols[0] : undefined
})

// ── API keys ─────────────────────────────────────────────────
const apiKeys = ref<string[]>([...props.provider.api_keys])
const keyStrategy = ref<"fill_first" | "round_robin">(props.provider.key_strategy)

// ── Models ───────────────────────────────────────────────────
const enabledModels = ref<string[]>([...props.provider.enabled_models])

function onTestModel(model: string) {
  const probe = probeApi.value
  if (!probe) {
    store.upstreamModelsError = "Declare an API section first (APIs tab)"
    return
  }
  store.testModelWithState(model, {
    model,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    base_url: probe.base_url,
    api_kind: probeKind.value ?? undefined,
    headers: probe.headers ?? headersMap(),
    proxy_mode: proxyMode.value,
  })
}

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

// ── Per-model body overrides ────────────────────────────────
const modelOverridesMap = ref<Record<string, { key: string; value: string }[]>>(
  Object.fromEntries(
    Object.entries(props.provider.model_overrides ?? {}).map(([model, overrides]) => [
      model,
      Object.entries(overrides).map(([k, v]) => ({
        key: k,
        value: typeof v === "string" ? v : JSON.stringify(v),
      })),
    ]),
  ),
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

// ── Fetch upstream models ────────────────────────────────────
function onFetchModels() {
  const probe = probeApi.value
  if (!probe) {
    store.upstreamModelsError = "Declare an API section first (APIs tab)"
    return
  }
  store.fetchUpstreamModels({
    base_url: probe.base_url,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    headers: probe.headers ?? headersMap(),
    api_kind: probeKind.value ?? undefined,
    models_endpoint: probe.models_endpoint,
    proxy_mode: proxyMode.value,
  })
}

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
        apiError.value = `Body override "${key}": invalid JSON value`
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

    // Parse model_overrides
    const parsedModelOverrides: Record<string, Record<string, any>> = {}
    for (const [model, pairs] of Object.entries(modelOverridesMap.value)) {
      const overrides: Record<string, any> = {}
      for (const { key, value: val } of pairs) {
        if (!key.trim()) continue
        try {
          overrides[key.trim()] = val.trim() ? JSON.parse(val) : val
        } catch {
          apiError.value = `Model override "${model}.${key}": invalid JSON value`
          return
        }
      }
      if (Object.keys(overrides).length > 0) {
        parsedModelOverrides[model] = overrides
      }
    }
    if (
      JSON.stringify(parsedModelOverrides) !== JSON.stringify(props.provider.model_overrides ?? {})
    ) {
      payload.model_overrides = parsedModelOverrides
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
    toast.success(`Provider "${props.provider.name}" updated`)
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
        <component :is="Modal.Description"> Edit provider configuration </component>
      </component>

      <div
        v-if="apiError"
        class="mx-6 mb-2 shrink-0 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500"
      >
        {{ apiError }}
      </div>

      <div class="flex-1 overflow-y-auto px-2 pb-4 sm:px-6">
        <Tabs default-value="basic">
          <TabsList class="mb-4">
            <TabsTrigger value="basic">Basic</TabsTrigger>
            <TabsTrigger value="apis">APIs</TabsTrigger>
            <TabsTrigger value="models">Models</TabsTrigger>
            <TabsTrigger value="advanced">Advanced</TabsTrigger>
          </TabsList>

          <!-- ─── Tab: Basic ──────────────────────────────────── -->
          <TabsContent value="basic" class="grid grid-cols-2 gap-4">
            <div>
              <FieldLabel>Name</FieldLabel>
              <Input :model-value="provider.name" disabled class="bg-muted text-muted-foreground" />
            </div>

            <div class="space-y-1">
              <FieldLabel>Timeout (secs)</FieldLabel>
              <Input
                type="number"
                :model-value="timeoutSecs"
                @input="timeoutSecs = Number(($event.target as HTMLInputElement).value)"
              />
            </div>

            <div class="col-span-2">
              <ApiKeyEditor
                :api-keys="apiKeys"
                :key-strategy="keyStrategy"
                @update:api-keys="apiKeys = $event"
                @update:key-strategy="keyStrategy = $event"
              />
            </div>

            <div class="space-y-1">
              <FieldLabel>Proxy Mode</FieldLabel>
              <Select
                :model-value="proxyMode"
                @update:model-value="(v) => (proxyMode = v as ProxyMode)"
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="inherit">Inherit</SelectItem>
                  <SelectItem value="enabled">Enabled</SelectItem>
                  <SelectItem value="disabled">Disabled</SelectItem>
                </SelectContent>
              </Select>
              <p class="text-muted-foreground text-xs">
                How this provider resolves the upstream proxy. Inherit follows the gateway-level
                proxy URL.
              </p>
            </div>
          </TabsContent>

          <!-- ─── Tab: APIs ───────────────────────────────────── -->
          <TabsContent value="apis" class="space-y-4">
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
            <div v-if="probeApi" class="flex min-w-0 items-center gap-2">
              <Select
                v-if="protocolSections.length > 1"
                :model-value="probeKind ?? undefined"
                @update:model-value="(v) => (probeKind = v as ApiKind)"
              >
                <SelectTrigger class="h-7 w-44 text-xs">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem
                    v-for="kind in protocolSections"
                    :key="kind"
                    :value="kind"
                    class="text-xs"
                  >
                    {{ API_KIND_LABELS[kind] }}
                  </SelectItem>
                </SelectContent>
              </Select>
              <code v-else class="text-xs">{{ probeKind }}</code>
              <span class="text-muted-foreground shrink-0 text-xs">→</span>
              <code class="truncate text-xs">
                {{ probeApi.base_url }}/{{ probeApi.models_endpoint || "models" }}
              </code>
            </div>
            <ModelManager
              :provider-name="provider.name"
              :enabled-models="enabledModels"
              :upstream-models="store.upstreamModels"
              :upstream-models-loading="store.upstreamModelsLoading"
              :upstream-models-error="store.upstreamModelsError"
              :model-test-states="store.testModelState.states"
              v-model:model-overrides="modelOverridesMap"
              :upstream-protocol="defaultProtocol"
              @update:enabled-models="enabledModels = $event"
              @fetch-models="onFetchModels"
              @test-model="onTestModel"
            />
          </TabsContent>

          <!-- ─── Tab: Advanced ───────────────────────────────── -->
          <TabsContent value="advanced" class="space-y-4">
            <p class="text-muted-foreground text-xs">
              Provider-level defaults below apply to every API section. A field declared inside an
              <strong>APIs</strong> section <strong>fully replaces</strong> the default of the same
              name for that section (maps are not merged, arrays not unioned).
            </p>

            <KvEditor
              v-model="headersList"
              title="Headers"
              description="Key-value pairs of custom headers (values are plain text)."
              key-placeholder="Header-Name"
              value-placeholder="Value"
            />

            <Separator />

            <Alert variant="warning">
              <AlertTitle>提示</AlertTitle>
              <AlertDescription>
                Body Overrides
                对该供应商的<strong>所有模型</strong>生效。如需为特定模型设置不同参数，建议前往
                <strong>Models Tab</strong> 配置 <strong>Model Overrides</strong>，避免在多个路由的
                target 中重复配置 <code>extra</code>。
              </AlertDescription>
            </Alert>

            <KvEditor
              v-model="bodyOverridesList"
              title="Body Overrides"
              description='Key-value pairs overridden in the upstream JSON body. Values are parsed as JSON (e.g. <code>4096</code>, <code>true</code>, <code>"text"</code>).'
              key-placeholder="max_tokens"
              value-placeholder="4096"
            />

            <Separator />

            <div>
              <FieldLabel>Strip Fields</FieldLabel>
              <p class="text-muted-foreground mb-2 text-xs">
                Top-level fields to remove from the upstream JSON body before sending. Applied last,
                after all overrides. Use for fields the upstream rejects (e.g.
                <code>chat_template_kwargs</code> for pydantic <code>extra=forbid</code>).
              </p>
              <div
                v-for="(_, idx) in stripFieldsList"
                :key="idx"
                class="mb-2 flex items-center gap-2"
              >
                <Input
                  :model-value="stripFieldsList[idx]"
                  placeholder="field_name"
                  @input="stripFieldsList[idx] = ($event.target as HTMLInputElement).value"
                />
                <Button variant="outline" size="icon-sm" @click="stripFieldsList.splice(idx, 1)">
                  <X class="h-3 w-3" />
                </Button>
              </div>
              <Button variant="outline" size="sm" @click="stripFieldsList.push('')">
                Add Field
              </Button>
            </div>

            <Separator />

            <KvEditor
              v-model="roleMappingsList"
              title="Role Mappings"
              description="Map request role names to role names supported by the upstream (case-sensitive). Example: developer → system."
              key-placeholder="developer"
              value-placeholder="system"
            />
          </TabsContent>
        </Tabs>
      </div>

      <component :is="Modal.Footer" class="shrink-0">
        <component :is="Modal.Close" as-child>
          <Button variant="outline" size="sm" :disabled="saving"> Cancel </Button>
        </component>
        <Button size="sm" :disabled="saving" @click="handleSubmit">
          <Loader2 v-if="saving" class="h-3 w-3 animate-spin" />
          {{ saving ? "Saving..." : "Save" }}
        </Button>
      </component>
    </component>
  </component>
</template>

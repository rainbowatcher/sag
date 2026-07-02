<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { useMediaQuery } from "@vueuse/core"
import { computed, ref } from "vue"
import { toast } from "vue-sonner"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import KvEditor from "@/components/KvEditor.vue"
import ModelManager from "@/components/ModelManager.vue"
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
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import type { Provider } from "@/types/api"

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

function onOpenChange(v: boolean) {
  if (!v) emit("close")
}

// ── Basic fields ─────────────────────────────────────────────
const providerType = ref(props.provider.provider_type)
const baseUrl = ref(props.provider.base_url)
const timeoutSecs = ref(props.provider.timeout_secs)

// ── API keys ─────────────────────────────────────────────────
const apiKeys = ref<string[]>([...props.provider.api_keys])
const keyStrategy = ref<"fill_first" | "round_robin">(props.provider.key_strategy)

// ── Models ───────────────────────────────────────────────────
const enabledModels = ref<string[]>([...props.provider.enabled_models])
const modelsEndpoint = ref(props.provider.models_endpoint || "models")

function defaultModelsEndpoint(providerType: string) {
  return providerType === "anthropic" ? "v1/models" : "models"
}

function onTestModel(model: string) {
  store.testModelWithState(model, {
    model,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    base_url: baseUrl.value,
    provider_type: providerType.value,
    headers: headersMap(),
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

// ── Fetch upstream models ────────────────────────────────────
function onFetchModels(endpoint: string) {
  store.fetchUpstreamModels({
    base_url: baseUrl.value,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    headers: headersMap(),
    provider_type: providerType.value,
    models_endpoint: endpoint,
  })
}

// ── Submit ───────────────────────────────────────────────────
async function handleSubmit() {
  apiError.value = ""
  saving.value = true
  try {
    const validKeys = apiKeys.value.filter((k) => k.trim())
    const payload: Record<string, any> = {}

    if (validKeys.length > 0) {
      payload.api_keys = validKeys
    }
    payload.key_strategy = keyStrategy.value

    const oldModelsEndpoint = defaultModelsEndpoint(props.provider.provider_type)
    if (
      providerType.value !== props.provider.provider_type &&
      modelsEndpoint.value === oldModelsEndpoint
    ) {
      modelsEndpoint.value = defaultModelsEndpoint(providerType.value)
    }

    if (providerType.value !== props.provider.provider_type) {
      payload.provider_type = providerType.value
    }
    if (baseUrl.value !== props.provider.base_url) {
      payload.base_url = baseUrl.value
    }
    if (timeoutSecs.value !== props.provider.timeout_secs) {
      payload.timeout_secs = timeoutSecs.value
    }

    const parsedHeaders = headersMap()
    if (JSON.stringify(parsedHeaders) !== JSON.stringify(props.provider.headers ?? {})) {
      payload.headers = parsedHeaders
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

    if (modelsEndpoint.value.trim() !== (props.provider.models_endpoint || "models")) {
      payload.models_endpoint = modelsEndpoint.value.trim() || undefined
    }

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
    <component :is="Modal.Content" class="sm:max-w-2xl">
      <component :is="Modal.Header">
        <component :is="Modal.Title">
          {{ provider.name }}
        </component>
        <component :is="Modal.Description"> Edit provider configuration </component>
      </component>

      <div v-if="apiError" class="mx-6 mb-2 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ apiError }}
      </div>

      <div class="px-2 pb-4 sm:px-6">
        <Tabs default-value="basic">
          <TabsList class="mb-4">
            <TabsTrigger value="basic">Basic</TabsTrigger>
            <TabsTrigger value="models">Models</TabsTrigger>
            <TabsTrigger value="advanced">Advanced</TabsTrigger>
          </TabsList>

          <!-- ─── Tab: Basic ──────────────────────────────────── -->
          <TabsContent value="basic" class="grid grid-cols-2 gap-4">
            <div>
              <FieldLabel>Name</FieldLabel>
              <Input :model-value="provider.name" disabled class="bg-muted text-muted-foreground" />
            </div>

            <div>
              <FieldLabel>Type</FieldLabel>
              <Select
                :model-value="providerType"
                @update:model-value="(v) => (providerType = v as string)"
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="openai">OpenAI</SelectItem>
                  <SelectItem value="anthropic">Anthropic</SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div class="col-span-2">
              <ApiKeyEditor
                :api-keys="apiKeys"
                :key-strategy="keyStrategy"
                @update:api-keys="apiKeys = $event"
                @update:key-strategy="keyStrategy = $event"
              />
            </div>

            <div class="col-span-2">
              <FieldLabel>Base URL</FieldLabel>
              <Input
                :model-value="baseUrl"
                @input="baseUrl = ($event.target as HTMLInputElement).value"
              />
            </div>

            <div class="space-y-1">
              <FieldLabel>Timeout (secs)</FieldLabel>
              <Input
                type="number"
                :model-value="timeoutSecs"
                @input="timeoutSecs = Number(($event.target as HTMLInputElement).value)"
              />
            </div>
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <ModelManager
              :enabled-models="enabledModels"
              :upstream-models="store.upstreamModels"
              :upstream-models-loading="store.upstreamModelsLoading"
              :upstream-models-error="store.upstreamModelsError"
              :model-test-states="store.testModelState.states"
              v-model:models-endpoint="modelsEndpoint"
              v-model:model-overrides="modelOverridesMap"
              @update:enabled-models="enabledModels = $event"
              @fetch-models="onFetchModels"
              @test-model="onTestModel"
            />
          </TabsContent>

          <!-- ─── Tab: Advanced ───────────────────────────────── -->
          <TabsContent value="advanced" class="space-y-4">
            <KvEditor
              v-model="headersList"
              title="Headers"
              description="Key-value pairs of custom headers (values are plain text)."
              key-placeholder="Header-Name"
              value-placeholder="Value"
            />

            <Separator />

            <KvEditor
              v-model="bodyOverridesList"
              title="Body Overrides"
              description='Key-value pairs overridden in the upstream JSON body. Values are parsed as JSON (e.g. <code>4096</code>, <code>true</code>, <code>"text"</code>).'
              key-placeholder="max_tokens"
              value-placeholder="4096"
            />
          </TabsContent>
        </Tabs>
      </div>

      <component :is="Modal.Footer">
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

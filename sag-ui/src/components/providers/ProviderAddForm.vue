<script setup lang="ts">
import { Loader2, X } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { computed, onMounted, ref, watch } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import KvEditor from "@/components/KvEditor.vue"
import ModelManager from "@/components/ModelManager.vue"
import { Button } from "@/components/ui/button"
import { Field, FieldError, FieldLabel } from "@/components/ui/field"
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
import type { ProxyMode, UpstreamProtocol } from "@/types/api"

const store = useProviderStore()

const emit = defineEmits<{
  close: []
  created: []
}>()

const adding = ref(false)
const apiError = ref("")

onMounted(() => {
  store.upstreamModels = []
  store.upstreamModelsError = ""
})

// ── API keys management ──────────────────────────────────────
const apiKeys = ref<string[]>([""])
const keyStrategy = ref<"fill_first" | "round_robin">("fill_first")

// ── Model management ─────────────────────────────────────────
const enabledModels = ref<string[]>([])
const modelsEndpoint = ref("models")

function defaultModelsEndpoint(providerType?: string) {
  return providerType === "anthropic" ? "v1/models" : "models"
}

function parsedHeadersValue(): Record<string, string> {
  return headers.value.reduce(
    (acc, h) => {
      if (h.key.trim() && h.value.trim()) acc[h.key.trim()] = h.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
}

function onFetchModels(endpoint: string) {
  store.fetchUpstreamModels({
    base_url: form.state.values.base_url,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    headers: parsedHeadersValue(),
    provider_type: form.state.values.provider_type,
    models_endpoint: endpoint,
    proxy_mode: proxyMode.value,
  })
}

function onTestModel(model: string) {
  store.testModelWithState(model, {
    model,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    base_url: form.state.values.base_url,
    provider_type: form.state.values.provider_type,
    headers: parsedHeadersValue(),
    proxy_mode: proxyMode.value,
  })
}

// ── Proxy mode ───────────────────────────────────────────────
// Inherit (default) follows the gateway-level proxy_url; never reuse a
// saved value — the form's current selection is always what gets sent.
const proxyMode = ref<ProxyMode>("inherit")

// ── Upstream protocol ────────────────────────────────────────
// "" = derive from provider_type (field omitted from the payload).
const upstreamProtocol = ref<UpstreamProtocol | "">("")
function deriveProtocol(type: string): string {
  return type === "anthropic" ? "anthropic_messages" : "chat_completions"
}
const providerEffectiveProtocol = computed(
  () => upstreamProtocol.value || deriveProtocol(form.state.values.provider_type),
)

// ── KV pairs ─────────────────────────────────────────────────
const headers = ref<{ key: string; value: string }[]>([])
const bodyOverrides = ref<{ key: string; value: string }[]>([])
const stripFields = ref<string[]>([])

// ── Per-model body overrides ────────────────────────────────
// Cleanup of stale entries is handled by ModelOverrideEditor internally.
const modelOverridesMap = ref<Record<string, { key: string; value: string }[]>>({})

// ── Base URL defaults per provider type ──────────────────────
const DEFAULT_BASE_URLS: Record<string, string> = {
  openai: "https://api.openai.com/v1",
  anthropic: "https://api.anthropic.com",
}

// ── Form ─────────────────────────────────────────────────────
const formSchema = z.object({
  name: z.string().min(1, "Name is required"),
  provider_type: z.string(),
  base_url: z.string(),
  timeout_secs: z.union([z.number(), z.undefined()]),
})

const form = useForm({
  defaultValues: {
    name: "",
    provider_type: "openai",
    base_url: "https://api.openai.com/v1",
    timeout_secs: undefined as number | undefined,
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""

    const validKeys = apiKeys.value.filter((k) => k.trim().length > 0)
    if (validKeys.length === 0) {
      apiError.value = "At least one API key is required"
      return
    }

    const parsedHeaders = parsedHeadersValue()

    const parsedOverrides = bodyOverrides.value.reduce(
      (acc, h) => {
        if (h.key.trim() && h.value.trim()) {
          try {
            acc[h.key.trim()] = JSON.parse(h.value.trim())
          } catch {
            acc[h.key.trim()] = h.value.trim()
          }
        }
        return acc
      },
      {} as Record<string, any>,
    )

    const parsedModelOverrides = Object.entries(modelOverridesMap.value).reduce(
      (acc, [model, pairs]) => {
        const overrides = pairs.reduce(
          (mAcc, h) => {
            if (h.key.trim() && h.value.trim()) {
              try {
                mAcc[h.key.trim()] = JSON.parse(h.value.trim())
              } catch {
                mAcc[h.key.trim()] = h.value.trim()
              }
            }
            return mAcc
          },
          {} as Record<string, any>,
        )
        if (Object.keys(overrides).length > 0) {
          acc[model] = overrides
        }
        return acc
      },
      {} as Record<string, Record<string, any>>,
    )

    const filteredStrip = stripFields.value.filter((f) => f.trim().length > 0)

    try {
      await store.addProvider({
        name: value.name,
        provider_type: value.provider_type,
        base_url: value.base_url,
        api_keys: validKeys,
        key_strategy: keyStrategy.value,
        enabled_models: enabledModels.value,
        headers: Object.keys(parsedHeaders).length > 0 ? parsedHeaders : undefined,
        body_overrides: Object.keys(parsedOverrides).length > 0 ? parsedOverrides : undefined,
        model_overrides:
          Object.keys(parsedModelOverrides).length > 0 ? parsedModelOverrides : undefined,
        strip_fields: filteredStrip.length > 0 ? filteredStrip : undefined,
        timeout_secs: value.timeout_secs,
        models_endpoint:
          modelsEndpoint.value &&
          modelsEndpoint.value !== defaultModelsEndpoint(value.provider_type)
            ? modelsEndpoint.value
            : undefined,
        proxy_mode: proxyMode.value,
        upstream_protocol: upstreamProtocol.value || undefined,
      })

      emit("created")
    } catch (e: any) {
      apiError.value = getErrorMessage(e)
      throw e
    }
  },
})

// Auto-fill base_url when provider_type changes
watch(
  () => form.state.values.provider_type,
  (newType) => {
    if (newType && DEFAULT_BASE_URLS[newType]) {
      form.setFieldValue("base_url", DEFAULT_BASE_URLS[newType])
    }
  },
)

function isInvalid(field: any): boolean {
  return field.state.meta.isTouched && field.state.meta.errors.length > 0
}

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  adding.value = true
  try {
    await form.handleSubmit()
  } catch (e: any) {
    if (!apiError.value) toast.error(getErrorMessage(e))
  } finally {
    adding.value = false
  }
}
</script>

<template>
  <div class="flex min-h-0 flex-1 flex-col overflow-hidden">
    <div v-if="apiError" class="mb-2 shrink-0 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
      {{ apiError }}
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto px-1 pb-4 sm:px-2">
      <form id="provider-add-form" @submit.prevent="handleSubmit">
        <Tabs default-value="basic">
          <TabsList class="mb-4">
            <TabsTrigger value="basic">Basic</TabsTrigger>
            <TabsTrigger value="models">Models</TabsTrigger>
            <TabsTrigger value="advanced">Advanced</TabsTrigger>
          </TabsList>

          <!-- ─── Tab: Basic ──────────────────────────────────── -->
          <TabsContent value="basic" class="grid grid-cols-2 gap-4">
            <form.Field name="name" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">Name *</FieldLabel>
                <Input
                  :id="field.name"
                  :model-value="field.state.value"
                  placeholder="my-openai"
                  :aria-invalid="isInvalid(field)"
                  @blur="field.handleBlur"
                  @input="field.handleChange(($event.target as HTMLInputElement).value)"
                />
                <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
              </Field>
            </form.Field>

            <form.Field name="provider_type" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">Type</FieldLabel>
                <Select
                  :model-value="field.state.value"
                  @update:model-value="(v) => field.handleChange(v as string)"
                >
                  <SelectTrigger :id="field.name">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent class="w-(--reka-select-trigger-width)">
                    <SelectItem value="openai">OpenAI</SelectItem>
                    <SelectItem value="anthropic">Anthropic</SelectItem>
                  </SelectContent>
                </Select>
                <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
              </Field>
            </form.Field>

            <div class="col-span-2">
              <ApiKeyEditor
                :api-keys="apiKeys"
                :key-strategy="keyStrategy"
                @update:api-keys="apiKeys = $event"
                @update:key-strategy="keyStrategy = $event"
              />
            </div>

            <div class="col-span-2">
              <form.Field name="base_url" #default="{ field }">
                <Field :data-invalid="isInvalid(field)">
                  <FieldLabel :for="field.name">Base URL</FieldLabel>
                  <Input
                    :id="field.name"
                    :model-value="field.state.value"
                    :aria-invalid="isInvalid(field)"
                    @blur="field.handleBlur"
                    @input="field.handleChange(($event.target as HTMLInputElement).value)"
                  />
                  <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
                </Field>
              </form.Field>
            </div>

            <div class="space-y-1">
              <form.Field name="timeout_secs" #default="{ field }">
                <Field :data-invalid="isInvalid(field)">
                  <FieldLabel :for="field.name">Timeout (secs)</FieldLabel>
                  <Input
                    :id="field.name"
                    type="number"
                    :model-value="field.state.value"
                    placeholder="600 (默认)"
                    :aria-invalid="isInvalid(field)"
                    @blur="field.handleBlur"
                    @input="
                      field.handleChange(
                        ($event.target as HTMLInputElement).value === ''
                          ? undefined
                          : Number(($event.target as HTMLInputElement).value),
                      )
                    "
                  />
                  <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
                </Field>
              </form.Field>
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

            <div class="space-y-1">
              <FieldLabel>Upstream Protocol</FieldLabel>
              <Select
                :model-value="upstreamProtocol"
                @update:model-value="(v) => (upstreamProtocol = v as UpstreamProtocol | '')"
              >
                <SelectTrigger>
                  <SelectValue placeholder="Derived from type" />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="">Derived from type</SelectItem>
                  <SelectItem value="chat_completions">Chat Completions</SelectItem>
                  <SelectItem value="responses">Responses</SelectItem>
                  <SelectItem value="anthropic_messages">Anthropic Messages</SelectItem>
                </SelectContent>
              </Select>
              <p class="text-muted-foreground text-xs">
                Explicit upstream protocol. Leave as derive unless the endpoint natively speaks a
                different protocol (e.g. Responses on an OpenAI-compatible base).
              </p>
            </div>
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <ModelManager
              :provider-name="form.state.values.name"
              :enabled-models="enabledModels"
              :upstream-models="store.upstreamModels"
              :upstream-models-loading="store.upstreamModelsLoading"
              :upstream-models-error="store.upstreamModelsError"
              :model-test-states="store.testModelState.states"
              v-model:models-endpoint="modelsEndpoint"
              v-model:model-overrides="modelOverridesMap"
              :upstream-protocol="providerEffectiveProtocol"
              @update:enabled-models="enabledModels = $event"
              @fetch-models="onFetchModels"
              @test-model="onTestModel"
            />
          </TabsContent>

          <!-- ─── Tab: Advanced ───────────────────────────────── -->
          <TabsContent value="advanced" class="space-y-4">
            <KvEditor
              v-model="headers"
              title="Headers"
              description="Key-value pairs of custom headers (values are plain text)."
              key-placeholder="Header-Name"
              value-placeholder="Value"
            />

            <Separator />

            <KvEditor
              v-model="bodyOverrides"
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
              <div v-for="(_, idx) in stripFields" :key="idx" class="mb-2 flex items-center gap-2">
                <Input
                  :model-value="stripFields[idx]"
                  placeholder="field_name"
                  @input="stripFields[idx] = ($event.target as HTMLInputElement).value"
                />
                <Button variant="outline" size="icon-sm" @click="stripFields.splice(idx, 1)">
                  <X class="h-3 w-3" />
                </Button>
              </div>
              <Button variant="outline" size="sm" @click="stripFields.push('')"> Add Field </Button>
            </div>
          </TabsContent>
        </Tabs>
      </form>
    </div>

    <div class="mt-auto flex shrink-0 justify-end gap-2 border-t pt-4">
      <Button variant="outline" size="sm" type="button" :disabled="adding" @click="emit('close')">
        Cancel
      </Button>
      <Button type="submit" form="provider-add-form" size="sm" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Provider" }}
      </Button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { Loader2, X } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { ref, watch } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import KvEditor from "@/components/KvEditor.vue"
import ModelManager from "@/components/ModelManager.vue"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
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

const store = useProviderStore()

const emit = defineEmits<{
  close: []
  created: []
}>()

const adding = ref(false)
const apiError = ref("")

// ── API keys management ──────────────────────────────────────
const apiKeys = ref<string[]>([""])
const keyStrategy = ref<"fill_first" | "round_robin">("fill_first")

// ── Model management ─────────────────────────────────────────
const enabledModels = ref<string[]>([])
const modelsEndpoint = ref("models")

function defaultModelsEndpoint(providerType?: string) {
  return providerType === "anthropic" ? "v1/models" : "models"
}

function onFetchModels(endpoint: string) {
  const parsedHeaders = headers.value.reduce(
    (acc, h) => {
      if (h.key.trim() && h.value.trim()) acc[h.key.trim()] = h.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
  store.fetchUpstreamModels({
    base_url: form.state.values.base_url,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    headers: parsedHeaders,
    provider_type: form.state.values.provider_type,
    models_endpoint: endpoint,
  })
}

// ── KV pairs ─────────────────────────────────────────────────
const headers = ref<{ key: string; value: string }[]>([])
const bodyOverrides = ref<{ key: string; value: string }[]>([])

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
    try {
      // Parse body overrides
      const parsedBodyOverrides: Record<string, any> = {}
      for (const { key, value: val } of bodyOverrides.value) {
        if (!key.trim()) continue
        try {
          parsedBodyOverrides[key.trim()] = val.trim() ? JSON.parse(val) : val
        } catch {
          apiError.value = `Body override "${key}": invalid JSON value`
          throw new Error("invalid body override JSON")
        }
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
            throw new Error("invalid model override JSON")
          }
        }
        if (Object.keys(overrides).length > 0) {
          parsedModelOverrides[model] = overrides
        }
      }

      // Build headers map
      const parsedHeaders: Record<string, string> = {}
      for (const { key, value: val } of headers.value) {
        if (key.trim() && val.trim()) {
          parsedHeaders[key.trim()] = val.trim()
        }
      }

      const validKeys = apiKeys.value.filter((k) => k.trim())

      // Omit timeout_secs when left blank so the backend applies its
      // default (600s) — single source of truth, no frontend/backend drift.
      const payload: Parameters<typeof store.addProvider>[0] = {
        name: value.name,
        api_keys: validKeys,
        key_strategy: keyStrategy.value,
        provider_type: value.provider_type,
        base_url: value.base_url || undefined,
        headers: parsedHeaders,
        body_overrides: parsedBodyOverrides,
        model_overrides: parsedModelOverrides,
        enabled_models: enabledModels.value,
        models_endpoint: modelsEndpoint.value.trim() || undefined,
      }
      await store.addProvider(payload)
      toast.success(`Provider "${value.name}" created`)
      emit("created")
    } catch (e: any) {
      const msg = getErrorMessage(e)
      apiError.value = msg
      toast.error(msg)
      throw e
    }
  },
})

// Watch provider_type: if base_url still matches a known default,
// auto-switch to the new type's default.
watch(
  () => form.state.values.provider_type,
  (newType, oldType) => {
    const currentUrl = form.state.values.base_url
    const isDefault = Object.values(DEFAULT_BASE_URLS).includes(currentUrl)
    if (isDefault && DEFAULT_BASE_URLS[newType]) {
      form.setFieldValue("base_url", DEFAULT_BASE_URLS[newType])
    }
    if (modelsEndpoint.value === defaultModelsEndpoint(oldType)) {
      modelsEndpoint.value = defaultModelsEndpoint(newType)
    }
  },
)

function isInvalid(field: any) {
  return field.state.meta.isTouched && !field.state.meta.isValid
}

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  adding.value = true
  try {
    await form.handleSubmit()
  } finally {
    adding.value = false
  }
}
</script>

<template>
  <Card class="animate-in slide-in-from-top-2">
    <CardHeader class="flex flex-row items-center justify-between space-y-0 py-4">
      <CardTitle>New Provider</CardTitle>
      <Button variant="ghost" size="icon-sm" @click="emit('close')">
        <X class="h-4 w-4" />
      </Button>
    </CardHeader>
    <CardContent>
      <div v-if="apiError" class="mb-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ apiError }}
      </div>
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
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <ModelManager
              :enabled-models="enabledModels"
              :upstream-models="store.upstreamModels"
              :upstream-models-loading="store.upstreamModelsLoading"
              :upstream-models-error="store.upstreamModelsError"
              v-model:models-endpoint="modelsEndpoint"
              v-model:model-overrides="modelOverridesMap"
              @update:enabled-models="enabledModels = $event"
              @fetch-models="onFetchModels"
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
          </TabsContent>
        </Tabs>
      </form>
    </CardContent>
    <CardFooter class="flex justify-end">
      <Button type="submit" form="provider-add-form" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Provider" }}
      </Button>
    </CardFooter>
  </Card>
</template>

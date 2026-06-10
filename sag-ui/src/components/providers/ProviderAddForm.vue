<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { Loader2, Plus, RefreshCw, X } from "lucide-vue-next"
import { ref, watch } from "vue"
import { z } from "zod"
import Combobox from "@/components/Combobox.vue"
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

function addApiKey() {
  apiKeys.value.push("")
}

function removeApiKey(idx: number) {
  apiKeys.value.splice(idx, 1)
  if (apiKeys.value.length === 0) apiKeys.value.push("")
}

// ── Model management ─────────────────────────────────────────
const enabledModels = ref<string[]>([])
const selectedModel = ref("")
const customModel = ref("")
const modelsEndpoint = ref("models")

function addModel() {
  const name = selectedModel.value.trim()
  if (name && !enabledModels.value.includes(name)) {
    enabledModels.value.push(name)
  }
  selectedModel.value = ""
}

function addCustomModel() {
  const name = customModel.value.trim()
  if (name && !enabledModels.value.includes(name)) {
    enabledModels.value.push(name)
  }
  customModel.value = ""
}

function removeModel(name: string) {
  const idx = enabledModels.value.indexOf(name)
  if (idx !== -1) enabledModels.value.splice(idx, 1)
}

// ── KV pair helpers ──────────────────────────────────────────
function addKvPair(arr: { key: string; value: string }[]) {
  arr.push({ key: "", value: "" })
}

function removeKvPair(arr: { key: string; value: string }[], idx: number) {
  arr.splice(idx, 1)
}

// ── Headers ──────────────────────────────────────────────────
const headers = ref<{ key: string; value: string }[]>([])

// ── Body overrides ───────────────────────────────────────────
const bodyOverrides = ref<{ key: string; value: string }[]>([])

// ── Base URL defaults per provider type ──────────────────────
const DEFAULT_BASE_URLS: Record<string, string> = {
  openai: "https://api.openai.com/v1",
  anthropic: "https://api.anthropic.com",
}

// Watch provider_type: if base_url still matches a known default,
// auto-switch to the new type's default.
watch(
  () => form.state.values.provider_type,
  (newType) => {
    const currentUrl = form.state.values.base_url
    // Only auto-switch if the current URL is a known default
    // (i.e. the user hasn't manually changed it)
    const isDefault = Object.values(DEFAULT_BASE_URLS).includes(currentUrl)
    if (isDefault && DEFAULT_BASE_URLS[newType]) {
      form.setFieldValue("base_url", DEFAULT_BASE_URLS[newType])
    }
  },
)

// ── V1 models ────────────────────────────────────────────────
const providerStore = useProviderStore()

function fetchModelsForForm() {
  const firstKey = apiKeys.value.find((k) => k.trim()) ?? ""
  const parsedHeaders = headers.value.reduce(
    (acc, h) => {
      if (h.key.trim() && h.value.trim()) acc[h.key.trim()] = h.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
  providerStore.fetchUpstreamModels({
    base_url: form.state.values.base_url,
    api_key: firstKey,
    headers: parsedHeaders,
    provider_type: form.state.values.provider_type,
    models_endpoint: modelsEndpoint.value,
  })
}

// ── Form ─────────────────────────────────────────────────────
const formSchema = z.object({
  name: z.string().min(1, "Name is required"),
  provider_type: z.string(),
  base_url: z.string(),
  timeout_secs: z.number(),
})

const form = useForm({
  defaultValues: {
    name: "",
    provider_type: "openai",
    base_url: "https://api.openai.com/v1",
    timeout_secs: 120,
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

      // Build headers map
      const parsedHeaders: Record<string, string> = {}
      for (const { key, value: val } of headers.value) {
        if (key.trim() && val.trim()) {
          parsedHeaders[key.trim()] = val.trim()
        }
      }

      const validKeys = apiKeys.value.filter((k) => k.trim())
      const effectiveApiKey = validKeys[0] || ""

      await store.addProvider({
        name: value.name,
        api_key: effectiveApiKey,
        api_keys: validKeys,
        key_strategy: keyStrategy.value,
        provider_type: value.provider_type,
        base_url: value.base_url || undefined,
        timeout_secs: value.timeout_secs,
        headers: parsedHeaders,
        body_overrides: parsedBodyOverrides,
        enabled_models: enabledModels.value,
      })
      emit("created")
    } catch (e: any) {
      if (apiError.value) return
      apiError.value = e.response?.data?.error?.message ?? e.message
      throw e
    }
  },
})

function isInvalid(field: any) {
  return field.state.meta.isTouched && !field.state.meta.isValid
}

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  adding.value = true
  try {
    await form.handleSubmit()
    if (apiError.value) return
    emit("created")
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

            <div class="col-span-2 space-y-1">
              <FieldLabel>API Keys</FieldLabel>
              <div v-for="(_, idx) in apiKeys" :key="idx" class="mb-1 flex items-center gap-2">
                <Input
                  type="password"
                  :model-value="apiKeys[idx]"
                  :placeholder="`sk-... (key ${idx + 1})`"
                  class="flex-1"
                  @input="apiKeys[idx] = ($event.target as HTMLInputElement).value"
                />
                <Button variant="ghost" size="icon-sm" @click="removeApiKey(idx)">
                  <X class="h-3.5 w-3.5" />
                </Button>
              </div>
              <Button type="button" variant="outline" size="sm" @click="addApiKey">
                <Plus class="h-3.5 w-3.5" />
                Add Key
              </Button>
            </div>

            <div class="col-span-2 space-y-1">
              <FieldLabel>Key Strategy</FieldLabel>
              <Select
                :model-value="keyStrategy"
                @update:model-value="(v) => (keyStrategy = v as any)"
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="fill_first">Fill First</SelectItem>
                  <SelectItem value="round_robin">Round Robin</SelectItem>
                </SelectContent>
              </Select>
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
                    :aria-invalid="isInvalid(field)"
                    @blur="field.handleBlur"
                    @input="field.handleChange(Number(($event.target as HTMLInputElement).value))"
                  />
                  <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
                </Field>
              </form.Field>
            </div>
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <p v-if="enabledModels.length === 0" class="text-muted-foreground text-xs">
              No models listed means all models are allowed (no restriction).
            </p>
            <div v-if="enabledModels.length > 0" class="space-y-1">
              <div
                v-for="model in enabledModels"
                :key="model"
                class="bg-muted flex items-center justify-between rounded px-2 py-1 text-xs"
              >
                <span class="font-mono">{{ model }}</span>
                <Button variant="ghost" size="icon-sm" @click="removeModel(model)">
                  <X class="h-3 w-3" />
                </Button>
              </div>
            </div>
            <div class="flex gap-2">
              <Combobox
                v-model="selectedModel"
                :options="
                  providerStore.v1Models
                    .filter((m) => !enabledModels.includes(m))
                    .map((m) => ({ value: m }))
                "
                placeholder="Select a model..."
                search-placeholder="Search models..."
                empty-text="No models found. Click Refresh Models."
                :loading="providerStore.v1ModelsLoading"
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                :disabled="!selectedModel.trim()"
                @click="addModel"
              >
                <Plus class="h-3.5 w-3.5" />
                Add
              </Button>
            </div>
            <div class="flex items-center gap-2">
              <Input
                :model-value="customModel"
                placeholder="Or type a model name..."
                class="flex-1"
                @input="customModel = ($event.target as HTMLInputElement).value"
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                :disabled="!customModel.trim()"
                @click="addCustomModel"
              >
                <Plus class="h-3.5 w-3.5" />
                Add
              </Button>
            </div>
            <div class="flex items-center gap-2">
              <Button type="button" variant="outline" size="sm" @click="fetchModelsForForm">
                <RefreshCw class="h-3.5 w-3.5" />
                Refresh Models
              </Button>
              <div v-if="providerStore.v1ModelsLoading" class="text-muted-foreground text-xs">
                Loading models...
              </div>
            </div>
            <div
              v-if="providerStore.v1ModelsError"
              class="rounded-md bg-red-50 px-2 py-1 text-xs text-red-500"
            >
              {{ providerStore.v1ModelsError }}
            </div>
            <div class="space-y-1">
              <FieldLabel>Models Endpoint Path</FieldLabel>
              <p class="text-muted-foreground text-xs">
                Path appended to base_url for fetching models (default: models).
              </p>
              <Input
                :model-value="modelsEndpoint"
                @input="modelsEndpoint = ($event.target as HTMLInputElement).value"
              />
            </div>
          </TabsContent>

          <!-- ─── Tab: Advanced ───────────────────────────────── -->
          <TabsContent value="advanced" class="space-y-4">
            <div class="space-y-1">
              <FieldLabel>Headers</FieldLabel>
              <p class="text-muted-foreground text-xs">
                Key-value pairs of custom headers (values are plain text).
              </p>
              <div
                v-for="(_, idx) in headers"
                :key="'hdr-' + idx"
                class="mb-1 flex items-center gap-2"
              >
                <Input
                  :model-value="headers[idx].key"
                  placeholder="Header-Name"
                  class="w-48"
                  @input="headers[idx].key = ($event.target as HTMLInputElement).value"
                />
                <Input
                  :model-value="headers[idx].value"
                  placeholder="Value"
                  class="flex-1"
                  @input="headers[idx].value = ($event.target as HTMLInputElement).value"
                />
                <Button variant="ghost" size="icon-sm" @click="removeKvPair(headers, idx)">
                  <X class="h-3.5 w-3.5" />
                </Button>
              </div>
              <Button type="button" variant="outline" size="sm" @click="addKvPair(headers)">
                <Plus class="h-3.5 w-3.5" />
                Add Header
              </Button>
            </div>

            <Separator />

            <div class="space-y-1">
              <FieldLabel>Body Overrides</FieldLabel>
              <p class="text-muted-foreground text-xs">
                Key-value pairs overridden in the upstream JSON body. Values are parsed as JSON
                (e.g. <code>4096</code>, <code>true</code>, <code>"text"</code>).
              </p>
              <div
                v-for="(_, idx) in bodyOverrides"
                :key="'bo-' + idx"
                class="mb-1 flex items-center gap-2"
              >
                <Input
                  :model-value="bodyOverrides[idx].key"
                  placeholder="max_tokens"
                  class="w-48"
                  @input="bodyOverrides[idx].key = ($event.target as HTMLInputElement).value"
                />
                <Input
                  :model-value="bodyOverrides[idx].value"
                  placeholder="4096"
                  class="flex-1"
                  @input="bodyOverrides[idx].value = ($event.target as HTMLInputElement).value"
                />
                <Button variant="ghost" size="icon-sm" @click="removeKvPair(bodyOverrides, idx)">
                  <X class="h-3.5 w-3.5" />
                </Button>
              </div>
              <Button type="button" variant="outline" size="sm" @click="addKvPair(bodyOverrides)">
                <Plus class="h-3.5 w-3.5" />
                Add Override
              </Button>
            </div>
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

<script setup lang="ts">
import { useMediaQuery } from "@vueuse/core"
import { Loader2, Plus, X as XIcon, RefreshCw } from "lucide-vue-next"
import { computed, onMounted, ref, type Ref } from "vue"
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
import { Field, FieldLabel } from "@/components/ui/field"
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

// ── API keys management ──────────────────────────────────────
const apiKeys = ref<string[]>([...props.provider.api_keys])
const keyStrategy = ref<"fill_first" | "round_robin">(props.provider.key_strategy)

function addApiKey() {
  apiKeys.value.push("")
}

function removeApiKey(idx: number) {
  apiKeys.value.splice(idx, 1)
  if (apiKeys.value.length === 0) apiKeys.value.push("")
}

// ── Model management ─────────────────────────────────────────
const enabledModels = ref<string[]>([...props.provider.enabled_models])
const selectedModel = ref("")
const modelsEndpoint = ref("models")
const customModel = ref("")

function addModelByName(name: string, sourceRef: Ref<string>) {
  const trimmed = name.trim()
  if (trimmed && !enabledModels.value.includes(trimmed)) {
    enabledModels.value.push(trimmed)
  }
  sourceRef.value = ""
}

function addModel() {
  addModelByName(selectedModel.value, selectedModel)
}

function addCustomModel() {
  addModelByName(customModel.value, customModel)
}

function removeModel(name: string) {
  const idx = enabledModels.value.indexOf(name)
  if (idx !== -1) enabledModels.value.splice(idx, 1)
}

// ── KV pair management ───────────────────────────────────────
const headersList = ref<{ key: string; value: string }[]>(
  Object.entries(props.provider.headers ?? {}).map(([k, v]) => ({ key: k, value: v })),
)
const bodyOverridesList = ref<{ key: string; value: string }[]>(
  Object.entries(props.provider.body_overrides ?? {}).map(([k, v]) => ({
    key: k,
    value: typeof v === "string" ? v : JSON.stringify(v),
  })),
)

function addKvPair(arr: { key: string; value: string }[]) {
  arr.push({ key: "", value: "" })
}

function removeKvPair(arr: { key: string; value: string }[], idx: number) {
  arr.splice(idx, 1)
}

function headersMap(): Record<string, string> {
  const m: Record<string, string> = {}
  for (const { key, value } of headersList.value) {
    if (key.trim() && value.trim()) m[key.trim()] = value.trim()
  }
  return m
}

// ── V1 models ────────────────────────────────────────────────
const providerStore = useProviderStore()

function runTestModel(model: string) {
  providerStore.testModelWithState(model, {
    model,
    api_keys:
      apiKeys.value.filter((k) => k.trim()).length > 0
        ? apiKeys.value.filter((k) => k.trim())
        : [props.provider.api_key],
    base_url: baseUrl.value,
    provider_type: providerType.value,
    headers: headersMap(),
  })
}

onMounted(() => {
  providerStore.fetchUpstreamModels({
    base_url: baseUrl.value,
    api_key: apiKeys.value[0] || props.provider.api_key,
    headers: headersMap(),
    provider_type: providerType.value,
    models_endpoint: modelsEndpoint.value,
  })
})

// ── Basic fields ─────────────────────────────────────────────
const providerType = ref(props.provider.provider_type)
const baseUrl = ref(props.provider.base_url)
const timeoutSecs = ref(props.provider.timeout_secs)
async function handleSubmit() {
  apiError.value = ""
  saving.value = true
  try {
    const validKeys = apiKeys.value.filter((k) => k.trim())
    const payload: Record<string, any> = {}

    if (validKeys.length > 0) {
      payload.api_key = validKeys[0]
      payload.api_keys = validKeys
    }
    payload.key_strategy = keyStrategy.value

    if (providerType.value !== props.provider.provider_type) {
      payload.provider_type = providerType.value
    }
    if (baseUrl.value !== props.provider.base_url) {
      payload.base_url = baseUrl.value
    }
    if (timeoutSecs.value !== props.provider.timeout_secs) {
      payload.timeout_secs = timeoutSecs.value
    }

    // Headers
    const parsedHeaders = headersMap()
    if (JSON.stringify(parsedHeaders) !== JSON.stringify(props.provider.headers ?? {})) {
      payload.headers = parsedHeaders
    }

    // Body overrides
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

    if (
      JSON.stringify(enabledModels.value) !== JSON.stringify(props.provider.enabled_models ?? [])
    ) {
      payload.enabled_models = enabledModels.value
    }

    await store.updateProvider(props.provider.name, payload)
    emit("updated")
  } catch (e: any) {
    if (apiError.value) return
    apiError.value = e.response?.data?.error?.message ?? e.message
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
                  <XIcon class="h-3.5 w-3.5" />
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
            <p v-if="enabledModels.length === 0" class="text-muted-foreground text-xs">
              No models listed means all models are allowed (no restriction).
            </p>
            <div v-if="enabledModels.length > 0" class="space-y-1">
              <div
                v-for="model in enabledModels"
                :key="model"
                class="bg-muted flex flex-wrap items-center gap-2 rounded px-2 py-1 text-xs"
              >
                <span class="font-mono">{{ model }}</span>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  :disabled="providerStore.testModelState.states[model]?.loading"
                  @click="runTestModel(model)"
                >
                  <Loader2
                    v-if="providerStore.testModelState.states[model]?.loading"
                    class="h-3 w-3 animate-spin"
                  />
                  <RefreshCw v-else class="h-3 w-3" />
                </Button>
                <div
                  v-if="providerStore.testModelState.states[model]?.data"
                  class="flex flex-wrap items-center gap-2"
                >
                  <span
                    v-for="r in providerStore.testModelState.states[model]?.data"
                    :key="r.key_prefix"
                    class="rounded px-1 py-0.5"
                    :class="r.success ? 'bg-green-100 text-green-700' : 'bg-red-100 text-red-700'"
                  >
                    {{ r.success ? "✓" : "✗" }} {{ r.key_prefix }}
                  </span>
                </div>
                <div
                  v-if="providerStore.testModelState.states[model]?.error"
                  class="w-full text-xs text-red-500"
                >
                  {{ providerStore.testModelState.states[model]?.error }}
                </div>
                <Button variant="ghost" size="icon-sm" @click="removeModel(model)" class="ml-auto">
                  <XIcon class="h-3 w-3" />
                </Button>
              </div>
            </div>
            <div class="flex gap-2">
              <Select
                :model-value="selectedModel"
                @update:model-value="(v) => (selectedModel = v as string)"
              >
                <SelectTrigger>
                  <SelectValue placeholder="Select a model..." />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem
                    v-for="m in providerStore.v1Models.filter((m) => !enabledModels.includes(m))"
                    :key="m"
                    :value="m"
                  >
                    {{ m }}
                  </SelectItem>
                </SelectContent>
              </Select>
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
              <Button
                type="button"
                variant="outline"
                size="sm"
                @click="
                  providerStore.fetchUpstreamModels({
                    base_url: baseUrl,
                    api_key: apiKeys[0] || props.provider.api_key,
                    headers: headersMap(),
                    provider_type: providerType,
                    models_endpoint: modelsEndpoint,
                  })
                "
              >
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
                v-for="(_, idx) in headersList"
                :key="'hdr-' + idx"
                class="mb-1 flex items-center gap-2"
              >
                <Input
                  :model-value="headersList[idx].key"
                  placeholder="Header-Name"
                  class="w-48"
                  @input="headersList[idx].key = ($event.target as HTMLInputElement).value"
                />
                <Input
                  :model-value="headersList[idx].value"
                  placeholder="Value"
                  class="flex-1"
                  @input="headersList[idx].value = ($event.target as HTMLInputElement).value"
                />
                <Button variant="ghost" size="icon-sm" @click="removeKvPair(headersList, idx)">
                  <XIcon class="h-3.5 w-3.5" />
                </Button>
              </div>
              <Button type="button" variant="outline" size="sm" @click="addKvPair(headersList)">
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
                v-for="(_, idx) in bodyOverridesList"
                :key="'bo-' + idx"
                class="mb-1 flex items-center gap-2"
              >
                <Input
                  :model-value="bodyOverridesList[idx].key"
                  placeholder="max_tokens"
                  class="w-48"
                  @input="bodyOverridesList[idx].key = ($event.target as HTMLInputElement).value"
                />
                <Input
                  :model-value="bodyOverridesList[idx].value"
                  placeholder="4096"
                  class="flex-1"
                  @input="bodyOverridesList[idx].value = ($event.target as HTMLInputElement).value"
                />
                <Button
                  variant="ghost"
                  size="icon-sm"
                  @click="removeKvPair(bodyOverridesList, idx)"
                >
                  <XIcon class="h-3.5 w-3.5" />
                </Button>
              </div>
              <Button
                type="button"
                variant="outline"
                size="sm"
                @click="addKvPair(bodyOverridesList)"
              >
                <Plus class="h-3.5 w-3.5" />
                Add Override
              </Button>
            </div>
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

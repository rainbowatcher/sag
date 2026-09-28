<script setup lang="ts">
import { Loader2, X } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { computed, onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import ApiKeyEditor from "@/components/ApiKeyEditor.vue"
import KvEditor from "@/components/KvEditor.vue"
import ModelManager from "@/components/ModelManager.vue"
import ApisEditor from "@/components/providers/ApisEditor.vue"
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
import type { ApiConfig, ApiKind, ProxyMode } from "@/types/api"

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

// ── API sections (protocol catalog) ──────────────────────────
const apis = ref<Partial<Record<ApiKind, ApiConfig>>>({})
const fallback = ref<ApiKind | null>(null)
const apisEditor = ref<{ validate: () => string | null } | null>(null)

/** The section model listing / model tests probe: chat_completions, else the first declared. */
const primaryKind = computed<ApiKind | null>(() => {
  const keys = Object.keys(apis.value) as ApiKind[]
  if (keys.includes("chat_completions")) return "chat_completions"
  return keys[0] ?? null
})
const primaryApi = computed<ApiConfig | null>(() =>
  primaryKind.value ? (apis.value[primaryKind.value] ?? null) : null,
)

/** Translated-lane protocol badge shown next to models. */
const defaultProtocol = computed<ApiKind | undefined>(() => {
  const protocols = (Object.keys(apis.value) as ApiKind[]).filter((k) => k !== "image")
  if (fallback.value) return fallback.value
  return protocols.length === 1 ? protocols[0] : undefined
})

// ── Model management ─────────────────────────────────────────
const enabledModels = ref<string[]>([])

function parsedHeadersValue(): Record<string, string> {
  return headers.value.reduce(
    (acc, h) => {
      if (h.key.trim() && h.value.trim()) acc[h.key.trim()] = h.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
}

/** Effective probe headers: section overrides fully replace provider-level defaults. */
function effectiveHeaders(): Record<string, string> {
  return primaryApi.value?.headers ?? parsedHeadersValue()
}

function roleMappingsMap(): Record<string, string> {
  return roleMappingsList.value.reduce(
    (acc, m) => {
      if (m.key.trim() && m.value.trim()) acc[m.key.trim()] = m.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
}

function onFetchModels() {
  const primary = primaryApi.value
  if (!primary) {
    store.upstreamModelsError = "Declare an API section first (APIs tab)"
    return
  }
  store.fetchUpstreamModels({
    base_url: primary.base_url,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    headers: effectiveHeaders(),
    api_kind: primaryKind.value ?? undefined,
    models_endpoint: primary.models_endpoint,
    proxy_mode: proxyMode.value,
  })
}

function onTestModel(model: string) {
  const primary = primaryApi.value
  if (!primary) {
    store.upstreamModelsError = "Declare an API section first (APIs tab)"
    return
  }
  store.testModelWithState(model, {
    model,
    api_keys: apiKeys.value.filter((k) => k.trim()),
    base_url: primary.base_url,
    api_kind: primaryKind.value ?? undefined,
    headers: effectiveHeaders(),
    proxy_mode: proxyMode.value,
  })
}

// ── Proxy mode ───────────────────────────────────────────────
// Inherit (default) follows the gateway-level proxy_url; never reuse a
// saved value — the form's current selection is always what gets sent.
const proxyMode = ref<ProxyMode>("inherit")

// ── KV pairs ─────────────────────────────────────────────────
const headers = ref<{ key: string; value: string }[]>([])
const roleMappingsList = ref<{ key: string; value: string }[]>([])
const bodyOverrides = ref<{ key: string; value: string }[]>([])
const stripFields = ref<string[]>([])

// ── Per-model body overrides ────────────────────────────────
// Cleanup of stale entries is handled by ModelOverrideEditor internally.
const modelOverridesMap = ref<Record<string, { key: string; value: string }[]>>({})

// ── Form ─────────────────────────────────────────────────────
const formSchema = z.object({
  name: z.string().min(1, "Name is required"),
  timeout_secs: z.union([z.number(), z.undefined()]),
})

const form = useForm({
  defaultValues: {
    name: "",
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

    const apisError = apisEditor.value?.validate()
    if (apisError) {
      apiError.value = apisError
      return
    }

    const parsedHeaders = parsedHeadersValue()
    const parsedRoleMappings = roleMappingsMap()

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
        api_keys: validKeys,
        key_strategy: keyStrategy.value,
        apis: apis.value as Record<ApiKind, ApiConfig>,
        ...(fallback.value ? { fallback: fallback.value } : {}),
        enabled_models: enabledModels.value,
        headers: Object.keys(parsedHeaders).length > 0 ? parsedHeaders : undefined,
        role_mappings: Object.keys(parsedRoleMappings).length > 0 ? parsedRoleMappings : undefined,
        body_overrides: Object.keys(parsedOverrides).length > 0 ? parsedOverrides : undefined,
        model_overrides:
          Object.keys(parsedModelOverrides).length > 0 ? parsedModelOverrides : undefined,
        strip_fields: filteredStrip.length > 0 ? filteredStrip : undefined,
        timeout_secs: value.timeout_secs,
        proxy_mode: proxyMode.value,
      })

      emit("created")
    } catch (e: any) {
      apiError.value = getErrorMessage(e)
      throw e
    }
  },
})

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
            <TabsTrigger value="apis">APIs</TabsTrigger>
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
              :provider-headers="parsedHeadersValue()"
            />
          </TabsContent>

          <!-- ─── Tab: Models ─────────────────────────────────── -->
          <TabsContent value="models" class="space-y-3">
            <p v-if="primaryApi" class="text-muted-foreground text-xs">
              Model listing probes
              <code>{{ primaryKind }}</code>
              → <code>{{ primaryApi.base_url }}</code
              ><template v-if="primaryApi.models_endpoint"
                >/{{ primaryApi.models_endpoint }}</template
              ><template v-else>/models</template>
              (edit on the APIs tab).
            </p>
            <ModelManager
              :provider-name="form.state.values.name"
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

            <Separator />

            <KvEditor
              v-model="roleMappingsList"
              title="Role Mappings"
              description="Maps request role names to roles the upstream supports (case-sensitive). Example: developer → system."
              key-placeholder="developer"
              value-placeholder="system"
            />
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

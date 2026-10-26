<script setup lang="ts">
import { Info, Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { useMediaQuery } from "@vueuse/core"
import { computed, ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import ApisEditor from "@/components/providers/ApisEditor.vue"
import ProviderAdvancedTab from "@/components/providers/ProviderAdvancedTab.vue"
import ProviderBasicFields from "@/components/providers/ProviderBasicFields.vue"
import ProviderModelsTab from "@/components/providers/ProviderModelsTab.vue"
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
import { Field, FieldError, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { useProviderProbe } from "@/composables/useProviderProbe"
import { t } from "@/lib/i18n"
import { getErrorMessage } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import type { ApiConfig, ApiKind, ProxyMode } from "@/types/api"

// Self-framing modal: Dialog on desktop, Drawer on mobile — mirrors the
// container mechanics of ProviderEditForm so both provider forms own theirs.
const isDesktop = useMediaQuery("(min-width: 640px)")

const Modal = computed(() => ({
  Root: isDesktop.value ? Dialog : Drawer,
  Content: isDesktop.value ? DialogContent : DrawerContent,
  Header: isDesktop.value ? DialogHeader : DrawerHeader,
  Title: isDesktop.value ? DialogTitle : DrawerTitle,
  Description: isDesktop.value ? DialogDescription : DrawerDescription,
  Footer: isDesktop.value ? DialogFooter : DrawerFooter,
  Close: isDesktop.value ? DialogClose : DrawerClose,
}))

const store = useProviderStore()

defineProps<{
  open?: boolean
}>()

const emit = defineEmits<{
  close: []
  created: []
}>()

const adding = ref(false)
const apiError = ref("")

function onOpenChange(v: boolean) {
  if (!v) emit("close")
}

// ── API keys management ──────────────────────────────────────
const apiKeys = ref<string[]>([""])
const keyStrategy = ref<"fill_first" | "round_robin">("fill_first")

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

function roleMappingsMap(): Record<string, string> {
  return roleMappingsList.value.reduce(
    (acc, m) => {
      if (m.key.trim() && m.value.trim()) acc[m.key.trim()] = m.value.trim()
      return acc
    },
    {} as Record<string, string>,
  )
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
  apiKeys,
  proxyMode,
  getHeaders: parsedHeadersValue,
})

// ── Form ─────────────────────────────────────────────────────
const form = useForm({
  defaultValues: {
    name: "",
    timeout_secs: undefined as number | undefined,
  },
  // `validators` is a getter (not a plain schema) so the messages re-resolve
  // through `t()` on every validation: form-core re-reads `options.validators`
  // on each run, so the schema is rebuilt after a locale switch.
  get validators() {
    return {
      onSubmit: z.object({
        name: z.string().min(1, t("providers.form.nameRequired")),
        timeout_secs: z.union([z.number(), z.undefined()]),
      }),
    }
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""

    const validKeys = apiKeys.value.filter((k) => k.trim().length > 0)
    if (validKeys.length === 0) {
      apiError.value = t("providers.form.apiKeyRequired")
      return
    }

    const apisError = apisEditor.value?.validate()
    if (apisError) {
      apiError.value = apisError
      return
    }

    const parsedHeaders = parsedHeadersValue()
    const parsedRoleMappings = roleMappingsMap()

    const parsedOverrides: Record<string, any> = {}
    for (const { key, value: val } of bodyOverrides.value) {
      if (!key.trim()) continue
      try {
        parsedOverrides[key.trim()] = val.trim() ? JSON.parse(val) : val
      } catch {
        apiError.value = t("providers.form.bodyOverrideInvalidJson", { key })
        return
      }
    }

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
  <component :is="Modal.Root" :open="open ?? true" @update:open="onOpenChange">
    <component :is="Modal.Content" class="flex max-h-[85vh] flex-col overflow-hidden sm:max-w-2xl">
      <component :is="Modal.Header" class="shrink-0">
        <component :is="Modal.Title" class="flex items-center gap-1.5">
          {{ $t("providers.addForm.title") }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
              {{ $t("providers.addForm.description") }}
            </TooltipContent>
          </Tooltip>
        </component>
        <!-- Visually hidden, keeps the dialog description for assistive tech. -->
        <component :is="Modal.Description" class="sr-only">
          {{ $t("providers.addForm.description") }}
        </component>
      </component>

      <div
        v-if="apiError"
        class="mx-6 mb-2 shrink-0 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500"
      >
        {{ apiError }}
      </div>

      <div class="min-h-0 min-w-0 flex-1 overflow-y-auto px-2 pb-4 sm:px-6">
        <form id="provider-add-form" @submit.prevent="handleSubmit">
          <Tabs default-value="basic">
            <TabsList class="mb-4">
              <TabsTrigger value="basic">{{ $t("providers.tabs.basic") }}</TabsTrigger>
              <TabsTrigger value="apis">{{ $t("providers.tabs.apis") }}</TabsTrigger>
              <TabsTrigger value="models">{{ $t("providers.tabs.models") }}</TabsTrigger>
              <TabsTrigger value="advanced">{{ $t("providers.tabs.advanced") }}</TabsTrigger>
            </TabsList>

            <!-- ─── Tab: Basic ──────────────────────────────────── -->
            <TabsContent value="basic" class="grid grid-cols-2 gap-4">
              <form.Field name="name" #default="{ field }">
                <Field :data-invalid="isInvalid(field)">
                  <div class="flex items-center justify-between gap-4">
                    <FieldLabel :for="field.name" class="shrink-0">{{
                      $t("providers.form.nameLabel")
                    }}</FieldLabel>
                    <Input
                      :id="field.name"
                      :model-value="field.state.value"
                      :placeholder="$t('providers.form.namePlaceholder')"
                      class="w-full max-w-56 min-w-0"
                      :aria-invalid="isInvalid(field)"
                      @blur="field.handleBlur"
                      @input="field.handleChange(($event.target as HTMLInputElement).value)"
                    />
                  </div>
                  <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
                </Field>
              </form.Field>

              <form.Field name="timeout_secs" #default="{ field }">
                <Field :data-invalid="isInvalid(field)">
                  <div class="flex items-center justify-between gap-4">
                    <FieldLabel :for="field.name" class="shrink-0">{{
                      $t("providers.form.timeoutLabel")
                    }}</FieldLabel>
                    <Input
                      :id="field.name"
                      type="number"
                      :model-value="field.state.value"
                      :placeholder="$t('providers.addForm.timeoutPlaceholder')"
                      class="w-24"
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
                  </div>
                  <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
                </Field>
              </form.Field>

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
                :provider-headers="parsedHeadersValue()"
              />
            </TabsContent>

            <!-- ─── Tab: Models ─────────────────────────────────── -->
            <TabsContent value="models" class="space-y-3">
              <ProviderModelsTab
                :provider-name="form.state.values.name"
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
                v-model:headers="headers"
                v-model:body-overrides="bodyOverrides"
                v-model:strip-fields="stripFields"
                v-model:role-mappings="roleMappingsList"
              />
            </TabsContent>
          </Tabs>
        </form>
      </div>

      <component :is="Modal.Footer" class="shrink-0">
        <component :is="Modal.Close" as-child>
          <Button variant="outline" size="sm" type="button" :disabled="adding">
            {{ $t("common.cancel") }}
          </Button>
        </component>
        <Button type="submit" form="provider-add-form" size="sm" :disabled="adding">
          <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
          {{ adding ? $t("providers.addForm.adding") : $t("providers.addForm.createProvider") }}
        </Button>
      </component>
    </component>
  </component>
</template>

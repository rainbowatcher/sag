<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { Loader2, Plus, X, Check, X as XIcon, RefreshCw } from "lucide-vue-next"
import { ref } from "vue"
import { z } from "zod"
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
import { api, headers as authHeaders } from "@/lib/client"
import { useConnectionStore } from "@/stores/connection"
import { useProviderStore } from "@/stores/providers"
import type { Provider } from "@/types/api"

const store = useProviderStore()

const props = defineProps<{
  provider: Provider
}>()

const emit = defineEmits<{
  close: []
  updated: []
}>()

const saving = ref(false)
const apiError = ref("")

// ── Model management ─────────────────────────────────────────────
const enabledModels = ref<string[]>([...props.provider.enabled_models])
const newModel = ref("")
const testingModel = ref<Record<string, boolean>>({})
const testModelResult = ref<Record<string, { success: boolean; message: string } | null>>({})

function addModel() {
  const name = newModel.value.trim()
  if (name && !enabledModels.value.includes(name)) {
    enabledModels.value.push(name)
  }
  newModel.value = ""
}

function removeModel(name: string) {
  const idx = enabledModels.value.indexOf(name)
  if (idx !== -1) enabledModels.value.splice(idx, 1)
}

async function testModel(model: string) {
  testingModel.value[model] = true
  testModelResult.value[model] = null
  try {
    const conn = useConnectionStore()
    const { data } = await api.post<{ success: boolean; message: string }>(
      "/api/model/test",
      {
        model,
        api_key: props.provider.api_key,
        base_url: props.provider.base_url,
        provider_type: props.provider.provider_type,
        headers: props.provider.headers,
      },
      { headers: authHeaders(conn.connection?.apiKey) },
    )
    testModelResult.value[model] = data
  } catch (e: any) {
    testModelResult.value[model] = {
      success: false,
      message: e.response?.data?.error?.message ?? e.message,
    }
  } finally {
    testingModel.value[model] = false
  }
}

const formSchema = z.object({
  api_key: z.string(),
  provider_type: z.string(),
  base_url: z.string(),
  priority: z.number(),
  max_retries: z.number(),
  timeout_secs: z.number(),
  supported_params: z.string(),
  headers_json: z.string(),
  body_overrides_json: z.string(),
})

const form = useForm({
  defaultValues: {
    api_key: "",
    provider_type: props.provider.provider_type,
    base_url: props.provider.base_url,
    priority: props.provider.priority,
    max_retries: props.provider.max_retries,
    timeout_secs: props.provider.timeout_secs,
    supported_params: props.provider.supported_params.join(", "),
    headers_json: JSON.stringify(props.provider.headers ?? {}, null, 2),
    body_overrides_json: JSON.stringify(props.provider.body_overrides ?? {}, null, 2),
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""
    try {
      const current = props.provider
      const payload: Record<string, any> = {}
      if (value.api_key) payload.api_key = value.api_key
      if (value.provider_type !== current.provider_type) {
        payload.provider_type = value.provider_type
      }
      if (value.base_url !== current.base_url) {
        payload.base_url = value.base_url
      }
      if (value.priority !== current.priority) {
        payload.priority = value.priority
      }
      if (value.max_retries !== current.max_retries) {
        payload.max_retries = value.max_retries
      }
      if (value.timeout_secs !== current.timeout_secs) {
        payload.timeout_secs = value.timeout_secs
      }
      const supported = value.supported_params
        ? value.supported_params
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean)
        : []
      const origSupported = current.supported_params ?? []
      if (JSON.stringify(supported) !== JSON.stringify(origSupported)) {
        payload.supported_params = supported
      }
      // Headers
      try {
        const parsed = value.headers_json ? JSON.parse(value.headers_json) : {}
        if (JSON.stringify(parsed) !== JSON.stringify(current.headers ?? {})) {
          payload.headers = parsed
        }
      } catch {
        apiError.value = "Headers must be valid JSON"
        throw new Error("invalid headers JSON")
      }
      // Body overrides
      try {
        const parsed = value.body_overrides_json ? JSON.parse(value.body_overrides_json) : {}
        if (JSON.stringify(parsed) !== JSON.stringify(current.body_overrides ?? {})) {
          payload.body_overrides = parsed
        }
      } catch {
        apiError.value = "Body overrides must be valid JSON"
        throw new Error("invalid body_overrides JSON")
      }
      if (JSON.stringify(enabledModels.value) !== JSON.stringify(current.enabled_models ?? [])) {
        payload.enabled_models = enabledModels.value
      }
      await store.updateProvider(current.name, payload)
      emit("updated")
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
  saving.value = true
  try {
    await form.handleSubmit()
    if (apiError.value) return
    emit("updated")
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <div class="space-y-3">
    <div v-if="apiError" class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
      {{ apiError }}
    </div>
    <form id="provider-edit-form" @submit.prevent="handleSubmit">
      <div class="grid grid-cols-2 gap-3">
        <div class="space-y-1">
          <FieldLabel class="text-muted-foreground text-xs font-medium">Name</FieldLabel>
          <Input :model-value="provider.name" disabled class="bg-muted text-muted-foreground" />
        </div>
        <form.Field name="provider_type" #default="{ field }">
          <Field :data-invalid="isInvalid(field)">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Type</FieldLabel
            >
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
          <form.Field name="api_key" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium">
                API Key (leave blank to keep current)
              </FieldLabel>
              <Input
                :id="field.name"
                type="password"
                :model-value="field.state.value"
                placeholder="Leave blank to keep current"
                :aria-invalid="isInvalid(field)"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>
        </div>

        <div class="col-span-2">
          <form.Field name="base_url" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
                >Base URL</FieldLabel
              >
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

        <form.Field name="priority" #default="{ field }">
          <Field :data-invalid="isInvalid(field)">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Priority</FieldLabel
            >
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

        <form.Field name="max_retries" #default="{ field }">
          <Field :data-invalid="isInvalid(field)">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Max Retries</FieldLabel
            >
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

        <form.Field name="timeout_secs" #default="{ field }">
          <Field :data-invalid="isInvalid(field)">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Timeout (secs)</FieldLabel
            >
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

        <div class="col-span-2">
          <form.Field name="supported_params" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
                >Supported Params</FieldLabel
              >
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="model,messages,temperature"
                :aria-invalid="isInvalid(field)"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>
        </div>

        <div class="col-span-2">
          <form.Field name="headers_json" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
                >Headers (JSON)</FieldLabel
              >
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder='{"Authorization":"Bearer custom","X-Custom":"value"}'
                :aria-invalid="isInvalid(field)"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground mt-0.5 text-xs">
                Optional JSON object of custom headers.
              </p>
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>
        </div>

        <div class="col-span-2">
          <form.Field name="body_overrides_json" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
                >Body Overrides (JSON)</FieldLabel
              >
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder='{"max_tokens":4096,"temperature":0.8}'
                :aria-invalid="isInvalid(field)"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground mt-0.5 text-xs">
                Optional JSON object of fields to override in the upstream request body.
              </p>
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>
        </div>

        <div class="col-span-2 border-t pt-3">
          <p class="text-muted-foreground mb-1 text-xs font-medium">Models</p>
          <p v-if="enabledModels.length === 0" class="text-muted-foreground mb-1 text-xs">
            No models listed means all models are allowed (no restriction).
          </p>
          <div v-if="enabledModels.length > 0" class="mb-2 space-y-1">
            <div
              v-for="model in enabledModels"
              :key="model"
              class="bg-muted flex items-center justify-between rounded px-2 py-1 text-xs"
            >
              <div class="flex items-center gap-2">
                <span class="font-mono">{{ model }}</span>
                <Button
                  variant="ghost"
                  size="icon-sm"
                  :disabled="testingModel[model]"
                  @click="testModel(model)"
                >
                  <Loader2 v-if="testingModel[model]" class="h-3 w-3 animate-spin" />
                  <RefreshCw v-else class="h-3 w-3" />
                </Button>
                <div
                  v-if="testModelResult[model]"
                  class="flex items-center gap-1 text-xs"
                  :class="testModelResult[model]?.success ? 'text-green-600' : 'text-red-600'"
                >
                  <Check v-if="testModelResult[model]?.success" class="h-3 w-3" />
                  <XIcon v-else class="h-3 w-3" />
                  {{ testModelResult[model]?.message }}
                </div>
              </div>
              <Button variant="ghost" size="icon-sm" @click="removeModel(model)">
                <X class="h-3 w-3" />
              </Button>
            </div>
          </div>
          <div class="flex gap-2">
            <Input
              :model-value="newModel"
              placeholder="gpt-4o"
              @input="newModel = ($event.target as HTMLInputElement).value"
              @keydown.enter.prevent="addModel"
            />
            <Button
              type="button"
              variant="outline"
              size="sm"
              :disabled="!newModel.trim()"
              @click="addModel"
            >
              <Plus class="h-3.5 w-3.5" />
              Add
            </Button>
          </div>
        </div>
      </div>
    </form>
    <div class="flex justify-end gap-2">
      <Button variant="outline" size="sm" @click="emit('close')" :disabled="saving">
        Cancel
      </Button>
      <Button type="submit" form="provider-edit-form" size="sm" :disabled="saving">
        <Loader2 v-if="saving" class="h-3 w-3 animate-spin" />
        {{ saving ? "Saving..." : "Save" }}
      </Button>
    </div>
  </div>
</template>

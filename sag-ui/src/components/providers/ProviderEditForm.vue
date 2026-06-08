<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { Loader2 } from "lucide-vue-next"
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

const formSchema = z.object({
  api_key: z.string(),
  provider_type: z.string(),
  base_url: z.string(),
  priority: z.number(),
  max_retries: z.number(),
  timeout_secs: z.number(),
  supported_params: z.string(),
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
      await store.updateProvider(current.name, payload)
      emit("updated")
    } catch (e: any) {
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
              <SelectContent>
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

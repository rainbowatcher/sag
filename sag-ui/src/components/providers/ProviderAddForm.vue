<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { X, Loader2, Plus } from "lucide-vue-next"
import { ref } from "vue"
import { z } from "zod"
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
import { useProviderStore } from "@/stores/providers"

const store = useProviderStore()

const emit = defineEmits<{
  close: []
  created: []
}>()

const adding = ref(false)
const apiError = ref("")

// ── Model management ─────────────────────────────────────────────
const enabledModels = ref<string[]>([])
const newModel = ref("")

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

const formSchema = z.object({
  name: z.string().min(1, "Name is required"),
  api_key: z.string().min(1, "API key is required"),
  provider_type: z.string(),
  base_url: z.string(),
  priority: z.number(),
  supported_params: z.string(),
  headers_json: z.string(),
  body_overrides_json: z.string(),
})

const form = useForm({
  defaultValues: {
    name: "",
    api_key: "",
    provider_type: "openai",
    base_url: "https://api.openai.com/v1",
    priority: 0,
    supported_params: "model,messages,temperature,max_tokens,stream",
    headers_json: "",
    body_overrides_json: "",
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""
    try {
      let headers: Record<string, string> = {}
      let bodyOverrides: Record<string, any> = {}
      try {
        headers = value.headers_json ? JSON.parse(value.headers_json) : {}
      } catch {
        apiError.value = "Headers must be valid JSON"
        throw new Error("invalid headers JSON")
      }
      try {
        bodyOverrides = value.body_overrides_json ? JSON.parse(value.body_overrides_json) : {}
      } catch {
        apiError.value = "Body overrides must be valid JSON"
        throw new Error("invalid body_overrides JSON")
      }

      await store.addProvider({
        name: value.name,
        api_key: value.api_key,
        provider_type: value.provider_type,
        base_url: value.base_url || undefined,
        priority: value.priority,
        supported_params: value.supported_params
          ? value.supported_params
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean)
          : [],
        headers,
        body_overrides: bodyOverrides,
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
        <div class="grid grid-cols-2 gap-4">
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
            <form.Field name="api_key" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">API Key *</FieldLabel>
                <Input
                  :id="field.name"
                  type="password"
                  :model-value="field.state.value"
                  placeholder="sk-..."
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

          <form.Field name="priority" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name">Priority</FieldLabel>
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

          <form.Field name="supported_params" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name">Supported Params</FieldLabel>
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

          <div class="col-span-2">
            <form.Field name="headers_json" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">Headers (JSON)</FieldLabel>
                <Input
                  :id="field.name"
                  :model-value="field.state.value"
                  placeholder='{"Authorization":"Bearer custom","X-Custom":"value"}'
                  :aria-invalid="isInvalid(field)"
                  @blur="field.handleBlur"
                  @input="field.handleChange(($event.target as HTMLInputElement).value)"
                />
                <p class="text-muted-foreground mt-0.5 text-xs">
                  Optional JSON object of custom headers to add or override.
                </p>
                <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
              </Field>
            </form.Field>
          </div>

          <div class="col-span-2">
            <form.Field name="body_overrides_json" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">Body Overrides (JSON)</FieldLabel>
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
                <span class="font-mono">{{ model }}</span>
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
    </CardContent>
    <CardFooter class="flex justify-end">
      <Button type="submit" form="provider-add-form" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Provider" }}
      </Button>
    </CardFooter>
  </Card>
</template>

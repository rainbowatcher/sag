<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { X, Loader2 } from "lucide-vue-next"
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

const formSchema = z.object({
  name: z.string().min(1, "Name is required"),
  api_key: z.string().min(1, "API key is required"),
  provider_type: z.string(),
  base_url: z.string(),
  priority: z.number(),
  supported_params: z.string(),
})

const form = useForm({
  defaultValues: {
    name: "",
    api_key: "",
    provider_type: "openai",
    base_url: "https://api.openai.com/v1",
    priority: 0,
    supported_params: "model,messages,temperature,max_tokens,stream",
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""
    try {
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
      })
      emit("created")
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

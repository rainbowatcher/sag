<script setup lang="ts">
import { useForm } from "@tanstack/vue-form"
import { X, Loader2, ChevronsUpDown, Check } from "lucide-vue-next"
import { ref, watch } from "vue"
import { z } from "zod"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import {
  Combobox,
  ComboboxAnchor,
  ComboboxInput,
  ComboboxTrigger,
  ComboboxList,
  ComboboxItem,
  ComboboxEmpty,
} from "@/components/ui/combobox"
import { Field, FieldError, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { useRouteStore } from "@/stores/routes"
import type { AddRoutePayload, Provider } from "@/types/api"

const routeStore = useRouteStore()

const props = defineProps<{
  providers: Provider[]
  providerModelsLoading: boolean
  providerModels: Record<string, string[]>
}>()

const emit = defineEmits<{
  close: []
  created: []
}>()

const open = ref(false)
const adding = ref(false)
const apiError = ref("")

const formSchema = z.object({
  provider_name: z.string().min(1, "Please select a provider"),
  model_name: z.string().min(1, "Model name is required"),
  real_model: z.string(),
  api_endpoint: z.string().min(1),
  priority: z.number(),
})

const form = useForm({
  defaultValues: {
    provider_name: "",
    model_name: "",
    real_model: "",
    api_endpoint: "/v1/chat/completions",
    priority: 0,
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""
    const payload: AddRoutePayload = {
      model_name: value.model_name,
      api_endpoint: value.api_endpoint || undefined,
      priority: value.priority,
    }
    if (value.real_model) {
      payload.real_model = value.real_model
    }
    try {
      await routeStore.addRoute(value.provider_name, payload)
    } catch (e: any) {
      apiError.value = e.response?.data?.error?.message ?? e.message
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

watch(
  () => form.state.values.provider_name,
  (name) => {
    form.setFieldValue("real_model", "")
    if (name) {
      routeStore.fetchProviderModels(name)
    }
  },
)

const endpointPresets = [
  { label: "Chat Completions", value: "/v1/chat/completions" },
  { label: "Responses (GPT-5)", value: "/v1/responses" },
  { label: "Image Generation", value: "/v1/images/generations" },
]
</script>

<template>
  <Card class="space-y-4">
    <CardHeader class="flex flex-row items-center justify-between space-y-0">
      <CardTitle>New Route</CardTitle>
      <Button variant="ghost" size="icon-sm" @click="emit('close')">
        <X class="h-4 w-4" />
      </Button>
    </CardHeader>
    <CardContent>
      <div v-if="apiError" class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ apiError }}
      </div>
      <form id="route-add-form" @submit.prevent="handleSubmit">
        <div class="grid grid-cols-2 gap-4">
          <form.Field name="provider_name" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name">Provider *</FieldLabel>
              <Select
                :model-value="field.state.value"
                @update:model-value="(v) => field.handleChange(v as string)"
              >
                <SelectTrigger :id="field.name" :aria-invalid="isInvalid(field)">
                  <SelectValue placeholder="Select provider..." />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem v-for="p in providers" :key="p.name" :value="p.name">
                    {{ p.name }} ({{ p.provider_type }})
                  </SelectItem>
                </SelectContent>
              </Select>
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

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

          <form.Field name="model_name" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name">Model Name *</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="gpt-4o"
                :aria-invalid="isInvalid(field)"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <form.Field name="real_model" #default="{ field }">
            <Field :data-invalid="isInvalid(field)">
              <FieldLabel :for="field.name">Real Model</FieldLabel>
              <Combobox
                :model-value="field.state.value"
                @update:model-value="(v) => field.handleChange(v as string)"
                v-model:open="open"
              >
                <ComboboxAnchor class="w-full">
                  <ComboboxInput
                    :id="field.name"
                    class="bg-background focus:ring-ring"
                    placeholder="Select a model"
                  />
                  <ComboboxTrigger>
                    <ChevronsUpDown class="h-4 w-4 shrink-0 opacity-50" />
                  </ComboboxTrigger>
                </ComboboxAnchor>
                <ComboboxList class="w-full">
                  <ComboboxEmpty>No models found</ComboboxEmpty>
                  <div
                    v-if="providerModelsLoading && !providerModels[form.state.values.provider_name]"
                    class="text-muted-foreground px-2 py-4 text-center text-xs"
                  >
                    Loading models...
                  </div>
                  <ComboboxItem
                    v-for="m in providerModels[form.state.values.provider_name] ?? []"
                    :key="m"
                    :value="m"
                  >
                    {{ m }}
                    <Check v-if="field.state.value === m" class="ml-auto h-4 w-4" />
                  </ComboboxItem>
                </ComboboxList>
              </Combobox>
              <p class="text-muted-foreground text-[10px]">
                Leave empty to use the client-requested model name as-is
              </p>
              <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <div class="col-span-2">
            <form.Field name="api_endpoint" #default="{ field }">
              <Field :data-invalid="isInvalid(field)">
                <FieldLabel :for="field.name">API Endpoint</FieldLabel>
                <ToggleGroup
                  :model-value="field.state.value"
                  @update:model-value="(v) => field.handleChange(v as string)"
                  type="single"
                  variant="outline"
                  size="sm"
                  class="justify-start"
                >
                  <ToggleGroupItem v-for="ep in endpointPresets" :key="ep.value" :value="ep.value">
                    {{ ep.label }}
                  </ToggleGroupItem>
                </ToggleGroup>
                <FieldError v-if="isInvalid(field)" :errors="field.state.meta.errors" />
              </Field>
            </form.Field>
          </div>
        </div>
      </form>
    </CardContent>
    <CardFooter class="flex justify-end">
      <Button type="submit" form="route-add-form" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Route" }}
      </Button>
    </CardFooter>
  </Card>
</template>

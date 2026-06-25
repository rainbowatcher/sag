<script setup lang="ts">
import { X, Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { getErrorMessage } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { Provider, Strategy, CreateRoutePayload, TargetItem } from "@/types/api"

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

const adding = ref(false)
const apiError = ref("")

const targets = ref<TargetItem[]>([
  { provider: "", real_model: "", priority: 0, retries: 0, extra: "" },
])

const endpointPresets = [
  { label: "Chat Completions", value: "/chat/completions" },
  { label: "Responses (GPT-5)", value: "/responses" },
  { label: "Image Generation", value: "/images/generations" },
]

const formSchema = z.object({
  model_name: z.string(),
  api_endpoint: z.string().min(1),
  strategy: z.enum(["sticky_failover", "fill_first", "round_robin"]),
})

const form = useForm({
  defaultValues: {
    model_name: "",
    api_endpoint: "/chat/completions",
    strategy: "sticky_failover" as Strategy,
  },
  validators: {
    onSubmit: formSchema,
  },
  onSubmit: async ({ value }) => {
    apiError.value = ""
    try {
      if (targets.value.length === 0) {
        apiError.value = "At least one target is required"
        return
      }
      const emptyTarget = targets.value.find((t) => !t.provider)
      if (emptyTarget) {
        apiError.value = "Each target must have a provider selected"
        return
      }

      const routeName = value.model_name || targets.value[0].real_model || ""
      if (!routeName) {
        apiError.value = "Please provide model_name or fill real_model on the first target"
        return
      }

      const payload: CreateRoutePayload = {
        model_name: routeName,
        api_endpoint: value.api_endpoint,
        strategy: value.strategy,
        targets: targets.value.map((t) => {
          const entry: CreateRoutePayload["targets"][number] = {
            provider: t.provider,
            real_model: t.real_model || undefined,
            priority: t.priority,
            retries: t.retries || undefined,
          }
          if (t.extra.trim()) {
            try {
              entry.extra = JSON.parse(t.extra)
            } catch {
              throw new Error(
                `Target ${t.provider || t.real_model || "#" + (targets.value.indexOf(t) + 1)}: invalid JSON in extra field`,
              )
            }
          }
          return entry
        }),
      }

      await routeStore.createRoute(payload)
    } catch (e: any) {
      apiError.value = getErrorMessage(e)
      throw e
    }
  },
})

function onCreated() {
  const name = form.state.values.model_name || targets.value[0].real_model || ""
  toast.success(`Route "${name}" created`)
}

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  adding.value = true
  try {
    await form.handleSubmit()
    if (apiError.value) return
    onCreated()
    emit("created")
  } catch (e: any) {
    if (!apiError.value) toast.error(getErrorMessage(e))
  } finally {
    adding.value = false
  }
}
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
      <div v-if="apiError" class="mb-4 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
        {{ apiError }}
      </div>
      <form id="route-add-form" @submit.prevent="handleSubmit">
        <div class="grid grid-cols-2 gap-4">
          <form.Field name="model_name" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">Route Alias</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="gpt-4o"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground text-[10px]">
                Leave empty to use the first target's real_model
              </p>
            </Field>
          </form.Field>

          <form.Field name="strategy" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">Strategy</FieldLabel>
              <ToggleGroup
                :model-value="field.state.value ?? 'sticky_failover'"
                @update:model-value="(v) => field.handleChange(v as Strategy)"
                type="single"
                variant="outline"
                size="sm"
                class="justify-start"
              >
                <ToggleGroupItem value="sticky_failover">Sticky-failover</ToggleGroupItem>
                <ToggleGroupItem value="fill_first">Fill-first</ToggleGroupItem>
                <ToggleGroupItem value="round_robin">Round-robin</ToggleGroupItem>
              </ToggleGroup>
            </Field>
          </form.Field>

          <div class="col-span-2">
            <form.Field name="api_endpoint" #default="{ field }">
              <Field>
                <FieldLabel :for="field.name">API Endpoint</FieldLabel>
                <ToggleGroup
                  :model-value="field.state.value ?? '/chat/completions'"
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
              </Field>
            </form.Field>
          </div>
        </div>

        <TargetEditor
          v-model="targets"
          :providers="providers"
          :provider-models="providerModels"
          :provider-models-loading="providerModelsLoading"
          show-priority-controls
        />
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

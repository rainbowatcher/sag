<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { getErrorMessage, validateExtraJson } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { Provider, Strategy, CreateRoutePayload, TargetItem } from "@/types/api"

const props = defineProps<{
  providers: Provider[]
  providerModelsLoading: boolean
  providerModels: Record<string, string[]>
}>()

const emit = defineEmits<{
  close: []
  created: []
}>()

const routeStore = useRouteStore()

const adding = ref(false)
const apiError = ref("")

const targets = ref<TargetItem[]>([
  { id: 1, provider: "", real_model: "", priority: 0, retries: 0, extra: "" },
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
            const extraErr = validateExtraJson(t.extra)
            if (extraErr) {
              throw new Error(
                `Target ${t.provider || t.real_model || "#" + (targets.value.indexOf(t) + 1)}: ${extraErr}`,
              )
            }
            entry.extra = JSON.parse(t.extra)
          }
          return entry
        }),
      }

      await routeStore.createRoute(payload)
      toast.success(`Route "${routeName}" created`)
      emit("created")
    } catch (e: any) {
      apiError.value = getErrorMessage(e)
      throw e
    }
  },
})

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
    <div v-if="apiError" class="mb-4 shrink-0 rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
      {{ apiError }}
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto px-1 py-1 sm:px-2">
      <form id="route-add-form" @submit.prevent="handleSubmit">
        <div class="grid grid-cols-2 gap-4">
          <form.Field name="model_name" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">Route Alias</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="gpt-4o"
                class="font-mono text-sm"
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
        />
      </form>
    </div>

    <div class="mt-4 flex shrink-0 justify-end gap-2 border-t pt-4">
      <Button variant="outline" size="sm" type="button" :disabled="adding" @click="emit('close')">
        Cancel
      </Button>
      <Button type="submit" form="route-add-form" size="sm" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? "Adding..." : "Create Route" }}
      </Button>
    </div>
  </div>
</template>

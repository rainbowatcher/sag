<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { onMounted, ref } from "vue"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig, Strategy, TargetItem } from "@/types/api"

const routeStore = useRouteStore()
const providerStore = useProviderStore()

const props = defineProps<{
  route: RouteConfig
}>()

const emit = defineEmits<{
  close: []
  updated: []
}>()

const saving = ref(false)
const apiError = ref("")
const targets = ref<TargetItem[]>(
  props.route.targets.map((t) => ({
    provider: t.provider,
    real_model: t.real_model ?? "",
    priority: t.priority,
    retries: t.retries ?? 0,
    extra: t.extra ? JSON.stringify(t.extra) : "",
  })),
)

// Pre-load models for existing targets
onMounted(() => {
  for (const t of targets.value) {
    if (t.provider && !routeStore.providerModels[t.provider]) {
      routeStore.loadProviderModels(t.provider, providerStore.providers)
    }
  }
})

function addTarget() {
  targets.value.push({ provider: "", real_model: "", priority: 0, retries: 0, extra: "" })
}

function removeTarget(index: number) {
  if (targets.value.length > 1) {
    targets.value.splice(index, 1)
  }
}

const endpointPresets = [
  { label: "Chat Completions", value: "/chat/completions" },
  { label: "Responses (GPT-5)", value: "/responses" },
  { label: "Image Generation", value: "/images/generations" },
]

const formSchema = z.object({
  api_endpoint: z.string().min(1),
  strategy: z.enum(["fill_first", "round_robin"]),
})

const form = useForm({
  defaultValues: {
    api_endpoint: props.route.api_endpoint,
    strategy: props.route.strategy,
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

      await routeStore.updateRoute({
        model_name: props.route.model_name,
        api_endpoint:
          value.api_endpoint !== props.route.api_endpoint ? value.api_endpoint : undefined,
        strategy: value.strategy !== props.route.strategy ? value.strategy : undefined,
        targets: targets.value.map((t) => {
          const entry: {
            provider: string
            real_model?: string
            priority?: number
            retries?: number
            extra?: Record<string, any>
          } = {
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
      })
      emit("updated")
    } catch (e: any) {
      apiError.value = e.response?.data?.error?.message ?? e.message
      throw e
    }
  },
})

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
  <div class="space-y-3 overflow-hidden">
    <div v-if="apiError" class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-500">
      {{ apiError }}
    </div>
    <form id="route-edit-form" @submit.prevent="handleSubmit">
      <div class="grid grid-cols-2 gap-3">
        <div class="space-y-1">
          <FieldLabel class="text-muted-foreground text-xs font-medium">Model Name</FieldLabel>
          <Input :model-value="route.model_name" disabled class="bg-muted text-muted-foreground" />
        </div>
        <form.Field name="strategy" #default="{ field }">
          <Field>
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Strategy</FieldLabel
            >
            <ToggleGroup
              :model-value="field.state.value"
              @update:model-value="(v) => field.handleChange(v as Strategy)"
              type="single"
              variant="outline"
              size="sm"
              class="justify-start"
            >
              <ToggleGroupItem value="fill_first">Fill-first</ToggleGroupItem>
              <ToggleGroupItem value="round_robin">Round-robin</ToggleGroupItem>
            </ToggleGroup>
          </Field>
        </form.Field>

        <div class="col-span-2">
          <form.Field name="api_endpoint" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
                >API Endpoint</FieldLabel
              >
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
            </Field>
          </form.Field>
        </div>
      </div>

      <div class="mt-4 space-y-3">
        <div class="flex items-center justify-between">
          <p class="text-sm font-medium">Targets</p>
          <Button type="button" variant="outline" size="sm" @click="addTarget">
            <Plus class="mr-1 h-3.5 w-3.5" />
            Add Target
          </Button>
        </div>

        <div v-for="(t, i) in targets" :key="i" class="flex items-end gap-2 rounded-lg border p-3">
          <span class="text-muted-foreground mb-2 w-5 pt-1 text-center text-xs font-medium">
            {{ i + 1 }}
          </span>

          <div class="flex-1">
            <Label class="text-xs">Provider</Label>
            <Combobox
              :model-value="t.provider"
              :options="
                providerStore.providers.map((p) => ({
                  value: p.name,
                  label: `${p.name} (${p.provider_type})`,
                }))
              "
              placeholder="Select provider..."
              search-placeholder="Search provider..."
              empty-text="No providers found"
              @update:model-value="
                (v) => {
                  t.provider = v as string
                  t.real_model = ''
                  routeStore.loadProviderModels(v as string, providerStore.providers)
                }
              "
            />
          </div>

          <div class="flex-1">
            <Label class="text-xs">Real Model</Label>
            <Combobox
              v-model="t.real_model"
              :options="(routeStore.providerModels[t.provider] ?? []).map((m) => ({ value: m }))"
              placeholder="Select model"
              search-placeholder="Search model..."
              empty-text="No models found"
              :loading="routeStore.providerModelsLoading && !routeStore.providerModels[t.provider]"
            />
          </div>

          <div class="w-20">
            <Label class="text-xs">Priority</Label>
            <Input
              type="number"
              :model-value="t.priority"
              @input="t.priority = Number(($event.target as HTMLInputElement).value)"
            />
          </div>

          <div class="w-20">
            <Label class="text-xs">Retries</Label>
            <Input
              type="number"
              :model-value="t.retries"
              @input="t.retries = Number(($event.target as HTMLInputElement).value)"
            />
          </div>

          <div class="w-36">
            <Label class="text-xs">Extra (JSON)</Label>
            <Input
              :model-value="t.extra"
              placeholder='{"model_id": 39}'
              @input="t.extra = ($event.target as HTMLInputElement).value"
            />
          </div>

          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            :disabled="targets.length === 1"
            @click="removeTarget(i)"
          >
            <Trash2 class="h-4 w-4" />
          </Button>
        </div>
      </div>
    </form>
    <div class="flex justify-end gap-2">
      <Button variant="outline" size="sm" @click="emit('close')" :disabled="saving">
        Cancel
      </Button>
      <Button type="submit" form="route-edit-form" size="sm" :disabled="saving">
        <Loader2 v-if="saving" class="h-3 w-3 animate-spin" />
        {{ saving ? "Saving..." : "Save" }}
      </Button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { t } from "@/lib/i18n"
import { getErrorMessage, validateExtraJson } from "@/lib/utils"
import { useProviderStore } from "@/stores/providers"
import { useRouteStore } from "@/stores/routes"
import type { RouteConfig, Strategy, Pipeline, TargetItem } from "@/types/api"

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
const submitted = ref(false)
const targets = ref<TargetItem[]>(
  props.route.targets.map((t, i) => ({
    id: i + 1,
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

const form = useForm({
  defaultValues: {
    pipeline: props.route.pipeline ?? ("chat" as Pipeline),
    api_endpoint: props.route.api_endpoint ?? "",
    strategy: props.route.strategy,
  },
  // `validators` is a getter so the refine message re-resolves through `t()`
  // on every validation (form-core re-reads `options.validators` each run).
  get validators() {
    return {
      onSubmit: z
        .object({
          pipeline: z.enum(["chat", "image"]),
          api_endpoint: z.string(),
          strategy: z.enum(["sticky_failover", "fill_first", "round_robin"]),
        })
        .refine((v) => v.pipeline !== "image" || v.api_endpoint.trim().length > 0, {
          message: t(
            "routes.form.apiEndpointRequired",
            undefined,
            "API endpoint is required for image pipeline",
          ),
          path: ["api_endpoint"],
        }),
    }
  },
  onSubmit: async ({ value }) => {
    submitted.value = true
    apiError.value = ""
    try {
      if (targets.value.length === 0) {
        apiError.value = t(
          "routes.form.targetRequired",
          undefined,
          "At least one target is required",
        )
        return
      }
      const emptyTarget = targets.value.find((target) => !target.provider)
      if (emptyTarget) {
        apiError.value = t(
          "routes.form.targetProviderRequired",
          undefined,
          "Each target must have a provider selected",
        )
        return
      }

      await routeStore.updateRoute({
        model_name: props.route.model_name,
        pipeline: value.pipeline,
        // Triple-state patch: an image route sends its endpoint; a chat
        // route sends `null` to clear a previously saved one.
        api_endpoint: value.pipeline === "image" ? value.api_endpoint.trim() : null,
        strategy: value.strategy !== props.route.strategy ? value.strategy : undefined,
        targets: targets.value.map((target) => {
          const entry: {
            provider: string
            real_model?: string
            priority?: number
            retries?: number
            extra?: Record<string, any>
          } = {
            provider: target.provider,
            real_model: target.real_model || undefined,
            priority: target.priority,
            retries: target.retries || undefined,
          }
          if (target.extra.trim()) {
            const extraErr = validateExtraJson(target.extra)
            if (extraErr) {
              const targetName =
                target.provider || target.real_model || "#" + (targets.value.indexOf(target) + 1)
              throw new Error(
                t(
                  "routes.form.targetExtra",
                  { name: targetName, error: extraErr },
                  `Target ${targetName}: ${extraErr}`,
                ),
              )
            }
            entry.extra = JSON.parse(target.extra)
          }
          return entry
        }),
      })
    } catch (e: any) {
      apiError.value = getErrorMessage(e)
      throw e
    }
  },
})

function onUpdated() {
  toast.success(
    t(
      "routes.toast.updated",
      { name: props.route.model_name },
      `Route "${props.route.model_name}" updated`,
    ),
  )
}

async function handleSubmit(event: Event) {
  event.preventDefault()
  apiError.value = ""
  saving.value = true
  submitted.value = false
  try {
    await form.handleSubmit()
    if (!submitted.value) return
    if (apiError.value) return
    onUpdated()
    emit("updated")
  } catch (e: any) {
    if (!apiError.value) toast.error(getErrorMessage(e))
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
        <div class="flex flex-col gap-3">
          <FieldLabel class="text-muted-foreground text-xs font-medium">Model Name</FieldLabel>
          <Input :model-value="route.model_name" disabled class="bg-muted text-muted-foreground" />
        </div>
        <form.Field name="strategy" #default="{ field }">
          <Field>
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Strategy</FieldLabel
            >
            <ToggleGroup
              :model-value="field.state.value ?? form.getFieldValue('strategy')"
              @update:model-value="(v) => field.handleChange(v as Strategy)"
              type="single"
              variant="outline"
              class="justify-start"
            >
              <ToggleGroupItem value="sticky_failover">Sticky-failover</ToggleGroupItem>
              <ToggleGroupItem value="fill_first">Fill-first</ToggleGroupItem>
              <ToggleGroupItem value="round_robin">Round-robin</ToggleGroupItem>
            </ToggleGroup>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>

        <form.Field name="pipeline" #default="{ field }">
          <Field>
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >Pipeline</FieldLabel
            >
            <ToggleGroup
              :model-value="field.state.value ?? 'chat'"
              @update:model-value="(v) => field.handleChange(v as Pipeline)"
              type="single"
              variant="outline"
              class="justify-start"
            >
              <ToggleGroupItem value="chat">Chat</ToggleGroupItem>
              <ToggleGroupItem value="image">Image</ToggleGroupItem>
            </ToggleGroup>
            <p class="text-muted-foreground mt-1 text-[10px]">
              Image routes send to the provider's image section with a per-route path.
            </p>
          </Field>
        </form.Field>

        <form.Field name="api_endpoint" #default="{ field }">
          <Field v-if="form.state.values.pipeline === 'image'">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium"
              >API Endpoint *</FieldLabel
            >
            <Input
              :id="field.name"
              :model-value="field.state.value"
              placeholder="/images/generations"
              class="font-mono text-sm"
              :aria-invalid="field.state.meta.errors.length > 0"
              @blur="field.handleBlur"
              @input="field.handleChange(($event.target as HTMLInputElement).value)"
            />
            <p class="text-muted-foreground mt-1 text-[10px]">
              Appended to the target provider's <code>image</code> section base_url, e.g.
              <code>/images/generations</code> or
              <code>/v1beta/models/{model}:generateContent</code>
            </p>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>
      </div>

      <TargetEditor
        v-model="targets"
        :providers="providerStore.providers"
        :provider-models="routeStore.providerModels"
        :provider-models-loading="routeStore.providerModelsLoading"
      />
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

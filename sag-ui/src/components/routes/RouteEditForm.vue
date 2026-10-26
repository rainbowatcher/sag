<script setup lang="ts">
import { Info, Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { computed, onMounted, ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
import { Button } from "@/components/ui/button"
import { Field, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
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

/** Empty input → `undefined` so the field is omitted and the current value is kept. */
function parseOptionalInt(raw: string): number | undefined {
  const trimmed = raw.trim()
  return trimmed === "" ? undefined : Number(trimmed)
}
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
      void routeStore.loadProviderModels(t.provider, providerStore.providers)
    }
  }
})

// reka-ui captures each `SelectItem`'s textContent once at mount and `SelectValue`
// renders that frozen string, so its default output never re-translates on a
// locale switch. This locale-reactive map feeds `SelectValue` an explicit label
// (same `t()` keys as the items) so it re-renders with the locale.
const strategyLabels = computed<Record<Strategy, string>>(() => ({
  sticky_failover: t("routes.form.strategyStickyFailover"),
  fill_first: t("routes.form.strategyFillFirst"),
  round_robin: t("routes.form.strategyRoundRobin"),
  session_sticky: t("routes.form.strategySessionSticky"),
}))

const form = useForm({
  defaultValues: {
    pipeline: props.route.pipeline ?? ("chat" as Pipeline),
    api_endpoint: props.route.api_endpoint ?? "",
    strategy: props.route.strategy,
    session_ttl_secs:
      props.route.session_ttl_secs === undefined ? "" : String(props.route.session_ttl_secs),
    session_max_entries:
      props.route.session_max_entries === undefined ? "" : String(props.route.session_max_entries),
  },
  // `validators` is a getter so the refine message re-resolves through `t()`
  // on every validation (form-core re-reads `options.validators` each run).
  get validators() {
    return {
      onSubmit: z
        .object({
          pipeline: z.enum(["chat", "image"]),
          api_endpoint: z.string(),
          strategy: z.enum(["sticky_failover", "fill_first", "round_robin", "session_sticky"]),
          session_ttl_secs: z.string(),
          session_max_entries: z.string(),
        })
        .refine((v) => v.pipeline !== "image" || v.api_endpoint.trim().length > 0, {
          message: t("routes.form.apiEndpointRequired"),
          path: ["api_endpoint"],
        })
        // Only validated for session_sticky: the inputs are hidden otherwise, so
        // a leftover value must not silently block submission.
        .refine(
          (v) =>
            v.strategy !== "session_sticky" ||
            v.session_ttl_secs.trim() === "" ||
            /^\d+$/.test(v.session_ttl_secs.trim()),
          { message: t("routes.form.sessionTtlInvalid"), path: ["session_ttl_secs"] },
        )
        .refine(
          (v) =>
            v.strategy !== "session_sticky" ||
            v.session_max_entries.trim() === "" ||
            /^[1-9]\d*$/.test(v.session_max_entries.trim()),
          { message: t("routes.form.sessionMaxEntriesInvalid"), path: ["session_max_entries"] },
        )
        // Rejected by the backend (`sag-config` route validation). The model
        // name is fixed on edit, so the wildcard is read from the route.
        .refine((v) => v.strategy !== "session_sticky" || props.route.model_name !== "*", {
          message: t("routes.form.wildcardSessionStickyUnsupported"),
          path: ["strategy"],
        })
        // Also a backend rejection: image dispatch performs no session
        // write-back, so a pin would never be recorded.
        .refine((v) => !(v.strategy === "session_sticky" && v.pipeline === "image"), {
          message: t("routes.form.imageSessionStickyUnsupported"),
          path: ["pipeline"],
        }),
    }
  },
  onSubmit: async ({ value }) => {
    submitted.value = true
    apiError.value = ""
    try {
      if (targets.value.length === 0) {
        apiError.value = t("routes.form.targetRequired")
        return
      }
      const emptyTarget = targets.value.find((target) => !target.provider)
      if (emptyTarget) {
        apiError.value = t("routes.form.targetProviderRequired")
        return
      }

      const ttl = parseOptionalInt(value.session_ttl_secs)
      const maxEntries = parseOptionalInt(value.session_max_entries)

      await routeStore.updateRoute({
        model_name: props.route.model_name,
        pipeline: value.pipeline,
        // Triple-state patch: an image route sends its endpoint; a chat
        // route sends `null` to clear a previously saved one.
        api_endpoint: value.pipeline === "image" ? value.api_endpoint.trim() : null,
        strategy: value.strategy !== props.route.strategy ? value.strategy : undefined,
        // Omitted → keep: the update DTO cannot clear these two back to
        // "unset", so only a changed value is sent.
        ...(value.strategy === "session_sticky"
          ? {
              session_ttl_secs: ttl !== props.route.session_ttl_secs ? ttl : undefined,
              session_max_entries:
                maxEntries !== props.route.session_max_entries ? maxEntries : undefined,
            }
          : {}),
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
              throw new Error(t("routes.form.targetExtra", { name: targetName, error: extraErr }))
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
  toast.success(t("routes.toast.updated", { name: props.route.model_name }))
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
          <FieldLabel class="text-muted-foreground text-xs font-medium">{{
            $t("routeList.columns.modelName")
          }}</FieldLabel>
          <Input :model-value="route.model_name" disabled class="bg-muted text-muted-foreground" />
        </div>
        <form.Field name="strategy" #default="{ field }">
          <Field>
            <FieldLabel
              :for="field.name"
              class="text-muted-foreground flex items-center gap-1.5 text-xs font-medium"
            >
              {{ $t("routes.form.strategy") }}
              <Tooltip v-if="form.state.values.strategy === 'session_sticky'">
                <TooltipTrigger as-child>
                  <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
                </TooltipTrigger>
                <TooltipContent side="top" class="max-w-xs font-normal">
                  {{ $t("routes.form.strategySessionStickyHint") }}
                </TooltipContent>
              </Tooltip>
            </FieldLabel>
            <Select
              :model-value="field.state.value ?? 'sticky_failover'"
              @update:model-value="(v) => field.handleChange(v as Strategy)"
            >
              <SelectTrigger :id="field.name" class="w-full">
                <SelectValue>{{
                  strategyLabels[field.state.value ?? "sticky_failover"]
                }}</SelectValue>
              </SelectTrigger>
              <SelectContent class="w-(--reka-select-trigger-width)">
                <SelectItem value="sticky_failover">{{
                  $t("routes.form.strategyStickyFailover")
                }}</SelectItem>
                <SelectItem value="fill_first">{{
                  $t("routes.form.strategyFillFirst")
                }}</SelectItem>
                <SelectItem value="round_robin">{{
                  $t("routes.form.strategyRoundRobin")
                }}</SelectItem>
                <SelectItem value="session_sticky">{{
                  $t("routes.form.strategySessionSticky")
                }}</SelectItem>
              </SelectContent>
            </Select>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>

        <form.Field name="pipeline" #default="{ field }">
          <Field>
            <FieldLabel
              :for="field.name"
              class="text-muted-foreground flex items-center gap-1.5 text-xs font-medium"
            >
              {{ $t("routes.form.pipeline") }}
              <Tooltip>
                <TooltipTrigger as-child>
                  <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
                </TooltipTrigger>
                <TooltipContent side="top" class="max-w-xs font-normal">
                  {{ $t("routes.form.pipelineHint") }}
                </TooltipContent>
              </Tooltip>
            </FieldLabel>
            <ToggleGroup
              :model-value="field.state.value ?? 'chat'"
              @update:model-value="(v) => field.handleChange(v as Pipeline)"
              type="single"
              variant="outline"
              class="justify-start"
            >
              <ToggleGroupItem value="chat">{{ $t("routes.form.pipelineChat") }}</ToggleGroupItem>
              <ToggleGroupItem value="image">{{ $t("routes.form.pipelineImage") }}</ToggleGroupItem>
            </ToggleGroup>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>

        <form.Field name="api_endpoint" #default="{ field }">
          <Field v-if="form.state.values.pipeline === 'image'">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium">{{
              $t("routes.form.apiEndpointLabel")
            }}</FieldLabel>
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
              {{ $t("routes.form.apiEndpointHintLead") }}<code>image</code
              >{{ $t("routes.form.apiEndpointHintMid") }}<code>/images/generations</code
              >{{ $t("routes.form.apiEndpointHintOr")
              }}<code>/v1beta/models/{model}:generateContent</code>
            </p>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>

        <form.Field name="session_ttl_secs" #default="{ field }">
          <Field v-if="form.state.values.strategy === 'session_sticky'">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium">{{
              $t("routes.form.sessionTtlLabel")
            }}</FieldLabel>
            <Input
              :id="field.name"
              type="number"
              min="0"
              :model-value="field.state.value"
              placeholder="0"
              class="font-mono text-sm"
              :aria-invalid="field.state.meta.errors.length > 0"
              @blur="field.handleBlur"
              @input="field.handleChange(($event.target as HTMLInputElement).value)"
            />
            <p class="text-muted-foreground mt-1 text-[10px]">
              {{ $t("routes.form.sessionTtlHint") }}
            </p>
            <p v-if="field.state.meta.errors.length" class="text-destructive mt-1 text-xs">
              {{ field.state.meta.errors[0] }}
            </p>
          </Field>
        </form.Field>

        <form.Field name="session_max_entries" #default="{ field }">
          <Field v-if="form.state.values.strategy === 'session_sticky'">
            <FieldLabel :for="field.name" class="text-muted-foreground text-xs font-medium">{{
              $t("routes.form.sessionMaxEntriesLabel")
            }}</FieldLabel>
            <Input
              :id="field.name"
              type="number"
              min="1"
              :model-value="field.state.value"
              placeholder="10000"
              class="font-mono text-sm"
              :aria-invalid="field.state.meta.errors.length > 0"
              @blur="field.handleBlur"
              @input="field.handleChange(($event.target as HTMLInputElement).value)"
            />
            <p class="text-muted-foreground mt-1 text-[10px]">
              {{ $t("routes.form.sessionMaxEntriesHint") }}
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
        {{ $t("common.cancel") }}
      </Button>
      <Button type="submit" form="route-edit-form" size="sm" :disabled="saving">
        <Loader2 v-if="saving" class="h-3 w-3 animate-spin" />
        {{ saving ? $t("common.saving") : $t("common.save") }}
      </Button>
    </div>
  </div>
</template>

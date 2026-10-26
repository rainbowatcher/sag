<script setup lang="ts">
import { Info, Loader2 } from "@lucide/vue"
import { useForm } from "@tanstack/vue-form"
import { computed, ref } from "vue"
import { toast } from "vue-sonner"
import { z } from "zod"
import TargetEditor from "@/components/routes/TargetEditor.vue"
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
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { getErrorMessage, validateExtraJson } from "@/lib/utils"
import { useRouteStore } from "@/stores/routes"
import type { Provider, Strategy, Pipeline, CreateRoutePayload, TargetItem } from "@/types/api"

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

// reka-ui captures each `SelectItem`'s textContent once at mount and `SelectValue`
// renders that frozen string, so its default output never re-translates on a
// locale switch. This locale-reactive map feeds `SelectValue` an explicit slot
// label (same `t()` keys as the items) instead of the captured text.
const strategyLabels = computed<Record<Strategy, string>>(() => ({
  sticky_failover: t("routes.form.strategyStickyFailover"),
  fill_first: t("routes.form.strategyFillFirst"),
  round_robin: t("routes.form.strategyRoundRobin"),
  session_sticky: t("routes.form.strategySessionSticky"),
}))

/** Empty input → `undefined` so the field is omitted and the backend default applies. */
function parseOptionalInt(raw: string): number | undefined {
  const trimmed = raw.trim()
  return trimmed === "" ? undefined : Number(trimmed)
}

// No seeded empty row: the target picker adds provider+model rows in bulk,
// and a stray blank row would block submission ("target provider required").
const targets = ref<TargetItem[]>([])

const form = useForm({
  defaultValues: {
    model_name: "",
    pipeline: "chat" as Pipeline,
    api_endpoint: "",
    strategy: "sticky_failover" as Strategy,
    session_ttl_secs: "",
    session_max_entries: "",
  },
  // `validators` is a getter so the refine message re-resolves through `t()`
  // on every validation (form-core re-reads `options.validators` each run).
  get validators() {
    return {
      onSubmit: z
        .object({
          model_name: z.string(),
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
        // Rejected by the backend (`sag-config` route validation) instead of
        // being stored: a wildcard route matches every request model, so the
        // session-affinity space keyed by request model is unbounded.
        .refine(
          (v) =>
            v.strategy !== "session_sticky" ||
            (v.model_name || targets.value[0]?.real_model || "") !== "*",
          { message: t("routes.form.wildcardSessionStickyUnsupported"), path: ["strategy"] },
        )
        // Also a backend rejection: image dispatch performs no session
        // write-back, so a pin would never be recorded.
        .refine((v) => !(v.strategy === "session_sticky" && v.pipeline === "image"), {
          message: t("routes.form.imageSessionStickyUnsupported"),
          path: ["pipeline"],
        }),
    }
  },
  onSubmit: async ({ value }) => {
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

      const routeName = value.model_name || targets.value[0].real_model || ""
      if (!routeName) {
        apiError.value = t("routes.form.modelNameRequired")
        return
      }

      const payload: CreateRoutePayload = {
        model_name: routeName,
        pipeline: value.pipeline,
        ...(value.pipeline === "image" ? { api_endpoint: value.api_endpoint.trim() } : {}),
        strategy: value.strategy,
        ...(value.strategy === "session_sticky"
          ? {
              session_ttl_secs: parseOptionalInt(value.session_ttl_secs),
              session_max_entries: parseOptionalInt(value.session_max_entries),
            }
          : {}),
        targets: targets.value.map((target) => {
          const entry: CreateRoutePayload["targets"][number] = {
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
      }

      await routeStore.createRoute(payload)
      toast.success(t("routes.toast.created", { name: routeName }))
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
              <FieldLabel :for="field.name">{{ $t("routes.form.routeAlias") }}</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="gpt-4o"
                class="font-mono text-sm"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground text-[10px]">
                {{ $t("routes.form.routeAliasHint") }}
              </p>
            </Field>
          </form.Field>

          <form.Field name="strategy" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">{{ $t("routes.form.strategy") }}</FieldLabel>
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
              <Tooltip v-if="form.state.values.strategy === 'session_sticky'">
                <TooltipTrigger as-child>
                  <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
                </TooltipTrigger>
                <TooltipContent side="top" class="max-w-xs font-normal">
                  {{ $t("routes.form.strategySessionStickyHint") }}
                </TooltipContent>
              </Tooltip>
              <FieldError v-if="field.state.meta.errors.length" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <form.Field name="pipeline" #default="{ field }">
            <Field>
              <FieldLabel :for="field.name">{{ $t("routes.form.pipeline") }}</FieldLabel>
              <ToggleGroup
                :model-value="field.state.value ?? 'chat'"
                @update:model-value="(v) => field.handleChange(v as Pipeline)"
                type="single"
                variant="outline"
                size="sm"
                class="justify-start"
              >
                <ToggleGroupItem value="chat">{{ $t("routes.form.pipelineChat") }}</ToggleGroupItem>
                <ToggleGroupItem value="image">{{
                  $t("routes.form.pipelineImage")
                }}</ToggleGroupItem>
              </ToggleGroup>
              <p class="text-muted-foreground text-[10px]">
                {{ $t("routes.form.pipelineHint") }}
              </p>
              <FieldError v-if="field.state.meta.errors.length" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <form.Field name="api_endpoint" #default="{ field }">
            <Field v-if="form.state.values.pipeline === 'image'">
              <FieldLabel :for="field.name">{{ $t("routes.form.apiEndpointLabel") }}</FieldLabel>
              <Input
                :id="field.name"
                :model-value="field.state.value"
                placeholder="/images/generations"
                class="font-mono text-sm"
                :aria-invalid="field.state.meta.errors.length > 0"
                @blur="field.handleBlur"
                @input="field.handleChange(($event.target as HTMLInputElement).value)"
              />
              <p class="text-muted-foreground text-[10px]">
                {{ $t("routes.form.apiEndpointHintLead") }}<code>image</code
                >{{ $t("routes.form.apiEndpointHintMid") }}<code>/images/generations</code
                >{{ $t("routes.form.apiEndpointHintOr")
                }}<code>/v1beta/models/{model}:generateContent</code>
              </p>
              <FieldError v-if="field.state.meta.errors.length" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <form.Field name="session_ttl_secs" #default="{ field }">
            <Field v-if="form.state.values.strategy === 'session_sticky'">
              <FieldLabel :for="field.name">{{ $t("routes.form.sessionTtlLabel") }}</FieldLabel>
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
              <p class="text-muted-foreground text-[10px]">
                {{ $t("routes.form.sessionTtlHint") }}
              </p>
              <FieldError v-if="field.state.meta.errors.length" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>

          <form.Field name="session_max_entries" #default="{ field }">
            <Field v-if="form.state.values.strategy === 'session_sticky'">
              <FieldLabel :for="field.name">{{
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
              <p class="text-muted-foreground text-[10px]">
                {{ $t("routes.form.sessionMaxEntriesHint") }}
              </p>
              <FieldError v-if="field.state.meta.errors.length" :errors="field.state.meta.errors" />
            </Field>
          </form.Field>
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
        {{ $t("common.cancel") }}
      </Button>
      <Button type="submit" form="route-add-form" size="sm" :disabled="adding">
        <Loader2 v-if="adding" class="h-3.5 w-3.5 animate-spin" />
        {{ adding ? $t("routes.addForm.adding") : $t("routes.addForm.createRoute") }}
      </Button>
    </div>
  </div>
</template>

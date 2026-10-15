<script setup lang="ts">
import { computed } from "vue"
import ModelManager from "@/components/ModelManager.vue"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { t } from "@/lib/i18n"
import { useProviderStore } from "@/stores/providers"
import type { ApiConfig, ApiKind } from "@/types/api"

defineProps<{
  providerName: string
  protocolSections: ApiKind[]
  probeApi: ApiConfig | null
  probeKind: ApiKind | null
  enabledModels: string[]
}>()

const emit = defineEmits<{
  "update:probeKind": [value: ApiKind | null]
  "update:enabledModels": [value: string[]]
  fetchModels: []
  testModel: [model: string]
}>()

const store = useProviderStore()

/** Display labels for `ApiKind` sections, resolved through `t()` (locale-reactive). */
const apiKindLabels = computed<Record<ApiKind, string>>(() => ({
  chat_completions: t("providers.probe.protocolChat"),
  responses: t("providers.probe.protocolResponses"),
  anthropic_messages: t("providers.probe.protocolAnthropic"),
  image: t("providers.apis.kindImage"),
}))
</script>

<template>
  <div v-if="probeApi" class="flex min-w-0 items-center gap-2">
    <Select
      v-if="protocolSections.length > 1"
      :model-value="probeKind ?? undefined"
      @update:model-value="(v) => emit('update:probeKind', v as ApiKind)"
    >
      <SelectTrigger class="h-7 w-44 text-xs">
        <SelectValue />
      </SelectTrigger>
      <SelectContent class="w-(--reka-select-trigger-width)">
        <SelectItem v-for="kind in protocolSections" :key="kind" :value="kind" class="text-xs">
          {{ apiKindLabels[kind] }}
        </SelectItem>
      </SelectContent>
    </Select>
    <code v-else class="text-xs">{{ probeKind }}</code>
    <span class="text-muted-foreground shrink-0 text-xs">→</span>
    <code class="truncate text-xs">
      {{ probeApi.base_url }}/{{ probeApi.models_endpoint || "models" }}
    </code>
  </div>
  <ModelManager
    :provider-name="providerName"
    :enabled-models="enabledModels"
    :upstream-models="store.upstreamModels"
    :upstream-models-loading="store.upstreamModelsLoading"
    :upstream-models-error="store.upstreamModelsError"
    :model-test-states="store.testModelState.states"
    @update:enabled-models="emit('update:enabledModels', $event)"
    @fetch-models="emit('fetchModels')"
    @test-model="(m) => emit('testModel', m)"
  />
</template>

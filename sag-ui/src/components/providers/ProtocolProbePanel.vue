<script setup lang="ts">
import { Info, Loader2, Search } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { useProviderStore } from "@/stores/providers"
import type { ProbeResult, ProbeVerdict, ProtocolApiKind, ProxyMode } from "@/types/api"

const PROTOCOL_KINDS: ProtocolApiKind[] = ["chat_completions", "responses", "anthropic_messages"]

const protocolLabels = computed<Record<ProtocolApiKind, string>>(() => ({
  chat_completions: t("providers.probe.protocolChat"),
  responses: t("providers.probe.protocolResponses"),
  anthropic_messages: t("providers.probe.protocolAnthropic"),
}))

/** Recover base_url from a probed URL by stripping the fixed protocol path. */
const PROTOCOL_PATHS: Record<ProtocolApiKind, string> = {
  chat_completions: "/chat/completions",
  responses: "/responses",
  anthropic_messages: "/messages",
}

const BACKFILL_VERDICTS: ReadonlySet<ProbeVerdict> = new Set<ProbeVerdict>([
  "supported",
  "auth_failed",
  "partial",
])

const verdictLabels = computed<Record<ProbeVerdict, string>>(() => ({
  supported: t("providers.probe.verdictSupported"),
  auth_failed: t("providers.probe.verdictAuthFailed"),
  uncertain: t("providers.probe.verdictUncertain"),
  unsupported: t("providers.probe.verdictUnsupported"),
  partial: t("providers.probe.verdictPartial"),
  error: t("providers.probe.verdictError"),
}))

const VERDICT_CLASSES: Record<ProbeVerdict, string> = {
  supported:
    "border-emerald-300 bg-emerald-50 text-emerald-700 dark:border-emerald-800 dark:bg-emerald-950 dark:text-emerald-400",
  auth_failed:
    "border-amber-300 bg-amber-50 text-amber-700 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-400",
  partial:
    "border-sky-300 bg-sky-50 text-sky-700 dark:border-sky-800 dark:bg-sky-950 dark:text-sky-400",
  uncertain:
    "border-zinc-300 bg-zinc-50 text-zinc-600 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400",
  unsupported:
    "border-red-300 bg-red-50 text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-400",
  error:
    "border-red-300 bg-red-50 text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-400",
}

const props = defineProps<{
  filledBaseUrls: Partial<Record<ProtocolApiKind, string>>
  sectionHeaders?: Partial<Record<ProtocolApiKind, Record<string, string>>>
  apiKeys: string[]
  proxyMode: ProxyMode
  headers: Record<string, string>
}>()

const emit = defineEmits<{
  backfill: [items: { protocol: ProtocolApiKind; base_url: string }[]]
}>()

const store = useProviderStore()

const model = ref("gpt-4o-mini")
const targets = ref<Record<ProtocolApiKind, string>>({
  chat_completions: "",
  responses: "",
  anthropic_messages: "",
})
const results = ref<ProbeResult[]>([])

// Prefill empty inputs only — never clobber an edited value.
watch(
  () => props.filledBaseUrls,
  (filled) => {
    for (const kind of PROTOCOL_KINDS) {
      const v = filled[kind]?.trim()
      if (v && !targets.value[kind].trim()) targets.value[kind] = v
    }
  },
  { immediate: true, deep: true },
)

function applyBaseToAll(kind: ProtocolApiKind) {
  const v = targets.value[kind].trim()
  if (!v) return
  for (const k of PROTOCOL_KINDS) {
    targets.value[k] = v
  }
}

const hasKeys = computed(() => props.apiKeys.some((k) => k.trim()))
const targetList = computed(() =>
  PROTOCOL_KINDS.map((protocol) => ({
    protocol,
    base_url: targets.value[protocol].trim(),
  })).filter((t) => t.base_url),
)
const canProbe = computed(
  () => hasKeys.value && targetList.value.length > 0 && !!model.value.trim(),
)

async function runProbe() {
  if (!canProbe.value) return
  results.value = await store.probeProtocols({
    api_keys: props.apiKeys.filter((k) => k.trim()),
    model: model.value.trim(),
    targets: targetList.value.map((t) => ({
      protocol: t.protocol,
      base_url: t.base_url,
      headers: props.sectionHeaders?.[t.protocol] ?? {},
    })),
    headers: props.headers,
    proxy_mode: props.proxyMode,
  })
}

const backfillItems = computed(() => {
  const seen = new Set<ProtocolApiKind>()
  const items: { protocol: ProtocolApiKind; base_url: string }[] = []
  for (const r of results.value) {
    if (!BACKFILL_VERDICTS.has(r.verdict) || seen.has(r.protocol)) continue
    seen.add(r.protocol)
    const suffix = PROTOCOL_PATHS[r.protocol]
    const base_url = r.url.endsWith(suffix) ? r.url.slice(0, -suffix.length) : r.url
    items.push({ protocol: r.protocol, base_url })
  }
  return items
})

function applyBackfill() {
  if (backfillItems.value.length === 0) return
  emit("backfill", backfillItems.value)
}
</script>

<template>
  <div class="min-w-0 space-y-3 rounded-md border p-3">
    <div class="space-y-2">
      <div v-for="kind in PROTOCOL_KINDS" :key="kind" class="space-y-1">
        <div class="flex items-center justify-between gap-2">
          <FieldLabel :for="`probe-base-${kind}`">
            {{ $t("providers.probe.baseUrlLabel", { protocol: protocolLabels[kind] }) }}
          </FieldLabel>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            class="h-7 text-xs"
            :disabled="!targets[kind].trim()"
            :title="$t('providers.probe.applyAllTitle')"
            @click="applyBaseToAll(kind)"
          >
            {{ $t("providers.probe.applyAll") }}
          </Button>
        </div>
        <Input
          :id="`probe-base-${kind}`"
          :model-value="targets[kind]"
          placeholder="https://api.example.com/v1"
          class="font-mono text-sm"
          @input="targets[kind] = ($event.target as HTMLInputElement).value"
        />
      </div>
    </div>

    <div class="flex flex-wrap items-end gap-2">
      <div class="min-w-40 flex-1 space-y-1">
        <FieldLabel for="probe-model">{{ $t("providers.probe.modelLabel") }}</FieldLabel>
        <Input
          id="probe-model"
          v-model="model"
          placeholder="gpt-4o-mini"
          class="font-mono text-sm"
        />
      </div>
      <Button type="button" size="sm" :disabled="!canProbe || store.probeLoading" @click="runProbe">
        <Loader2 v-if="store.probeLoading" class="h-3 w-3 animate-spin" />
        <Search v-else class="h-3.5 w-3.5" />
        {{ store.probeLoading ? $t("providers.probe.probing") : $t("providers.probe.probe") }}
      </Button>
      <Button
        v-if="results.length > 0"
        type="button"
        variant="outline"
        size="sm"
        :disabled="backfillItems.length === 0"
        @click="applyBackfill"
      >
        {{
          backfillItems.length === 1
            ? $t("providers.probe.backfillOne")
            : $t("providers.probe.backfillMany", { count: backfillItems.length })
        }}
      </Button>
    </div>

    <p v-if="!hasKeys" class="text-muted-foreground text-xs">
      {{ $t("providers.probe.addKeyFirst") }}
    </p>

    <p v-if="store.probeError" class="text-destructive text-xs">{{ store.probeError }}</p>

    <Alert v-if="results.length > 0" variant="default" class="min-w-0">
      <AlertTitle class="flex items-center gap-1.5">
        {{ $t("providers.probe.resultsTitle") }}
        <Tooltip>
          <TooltipTrigger as-child>
            <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
          </TooltipTrigger>
          <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
            {{ $t("providers.probe.backfillHint") }}
          </TooltipContent>
        </Tooltip>
      </AlertTitle>
      <AlertDescription class="min-w-0">
        <div class="w-full min-w-0 overflow-x-auto">
          <Table class="w-max min-w-full">
            <TableHeader>
              <TableRow>
                <TableHead>{{ $t("providers.probe.columns.protocol") }}</TableHead>
                <TableHead>{{ $t("providers.probe.columns.verdict") }}</TableHead>
                <TableHead>{{ $t("providers.probe.columns.key") }}</TableHead>
                <TableHead>{{ $t("providers.probe.columns.message") }}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="(r, i) in results" :key="i">
                <TableCell class="font-mono text-xs">{{ r.protocol }}</TableCell>
                <TableCell>
                  <Badge
                    variant="outline"
                    class="font-mono text-xs"
                    :class="VERDICT_CLASSES[r.verdict]"
                  >
                    {{ verdictLabels[r.verdict] }}
                  </Badge>
                </TableCell>
                <TableCell class="font-mono text-xs">{{ r.key_prefix }}</TableCell>
                <TableCell class="text-xs" :title="r.message">{{ r.message }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </div>
      </AlertDescription>
    </Alert>
  </div>
</template>

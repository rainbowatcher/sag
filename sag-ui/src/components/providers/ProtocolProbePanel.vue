<script setup lang="ts">
import { Loader2, Search } from "@lucide/vue"
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
import { useProviderStore } from "@/stores/providers"
import type { ProbeResult, ProbeVerdict, ProtocolApiKind, ProxyMode } from "@/types/api"

const PROTOCOL_KINDS: ProtocolApiKind[] = ["chat_completions", "responses", "anthropic_messages"]

const PROTOCOL_LABELS: Record<ProtocolApiKind, string> = {
  chat_completions: "Chat Completions",
  responses: "Responses",
  anthropic_messages: "Anthropic Messages",
}

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

const VERDICT_LABELS: Record<ProbeVerdict, string> = {
  supported: "supported",
  auth_failed: "auth failed",
  uncertain: "uncertain",
  unsupported: "unsupported",
  partial: "partial",
  error: "error",
}

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
    <p class="text-muted-foreground text-xs">
      Read-only probe: one minimal request per protocol × key. Nothing is saved until you backfill
      and store the provider.
    </p>

    <div class="space-y-2">
      <div v-for="kind in PROTOCOL_KINDS" :key="kind" class="space-y-1">
        <div class="flex items-center justify-between gap-2">
          <FieldLabel :for="`probe-base-${kind}`">{{ PROTOCOL_LABELS[kind] }} base URL</FieldLabel>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            class="h-7 text-xs"
            :disabled="!targets[kind].trim()"
            title="Apply this base URL to all protocols"
            @click="applyBaseToAll(kind)"
          >
            Apply to all
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
        <FieldLabel for="probe-model">Model *</FieldLabel>
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
        {{ store.probeLoading ? "Probing…" : "Probe" }}
      </Button>
      <Button
        v-if="results.length > 0"
        type="button"
        variant="outline"
        size="sm"
        :disabled="backfillItems.length === 0"
        @click="applyBackfill"
      >
        Backfill {{ backfillItems.length }} protocol{{ backfillItems.length === 1 ? "" : "s" }}
      </Button>
    </div>

    <p class="text-muted-foreground text-xs">
      Model: prefer a non-reasoning model. Use <strong>Apply to all</strong> to share one base URL
      across protocols.
    </p>
    <p v-if="!hasKeys" class="text-muted-foreground text-xs">Add at least one API key first.</p>

    <p v-if="store.probeError" class="text-destructive text-xs">{{ store.probeError }}</p>

    <Alert v-if="results.length > 0" variant="default" class="min-w-0">
      <AlertTitle>Probe results</AlertTitle>
      <AlertDescription class="min-w-0">
        <div class="w-full min-w-0 overflow-x-auto">
          <Table class="w-max min-w-full">
            <TableHeader>
              <TableRow>
                <TableHead>Protocol</TableHead>
                <TableHead>Verdict</TableHead>
                <TableHead>Key</TableHead>
                <TableHead>Message</TableHead>
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
                    {{ VERDICT_LABELS[r.verdict] }}
                  </Badge>
                </TableCell>
                <TableCell class="font-mono text-xs">{{ r.key_prefix }}</TableCell>
                <TableCell class="text-xs" :title="r.message">{{ r.message }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </div>
        <p class="text-muted-foreground mt-2 text-xs">
          Backfill fills matching sections only (supported / auth failed / partial).
        </p>
      </AlertDescription>
    </Alert>
  </div>
</template>

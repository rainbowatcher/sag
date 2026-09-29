<script setup lang="ts">
import { ChevronDown, ChevronRight, Plus, X } from "@lucide/vue"
import { computed, ref } from "vue"
import KvEditor from "@/components/KvEditor.vue"
import ProtocolProbePanel from "@/components/providers/ProtocolProbePanel.vue"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import type { ApiConfig, ApiKind, ProtocolApiKind, ProxyMode } from "@/types/api"

interface KvPair {
  key: string
  value: string
}

interface SectionDraft {
  uid: number
  kind: ApiKind
  base_url: string
  models_endpoint: string
  headers: KvPair[]
  body_overrides: KvPair[]
  strip_fields: string[]
  model_overrides: KvPair[]
  role_mappings: KvPair[]
  anthropic_cache_control: "inherit" | "enable" | "disable"
  timeout_secs: string
  chunk_timeout_secs: string
  ttfb_timeout_secs: string
  advancedOpen: boolean
}

const props = defineProps<{
  modelValue: Partial<Record<ApiKind, ApiConfig>>
  fallback: ApiKind | null
  apiKeys?: string[]
  proxyMode?: ProxyMode
  providerHeaders?: Record<string, string>
}>()

const emit = defineEmits<{
  "update:modelValue": [value: Record<ApiKind, ApiConfig>]
  "update:fallback": [value: ApiKind | null]
}>()

const API_KIND_LABELS: Record<ApiKind, string> = {
  chat_completions: "Chat Completions",
  responses: "Responses",
  anthropic_messages: "Anthropic Messages",
  image: "Image",
}

const API_KIND_ORDER: ApiKind[] = ["chat_completions", "responses", "anthropic_messages", "image"]

const PROTOCOL_KINDS = new Set<ApiKind>(["chat_completions", "responses", "anthropic_messages"])

const DEFAULT_MODELS_ENDPOINT_HINTS: Record<ApiKind, string> = {
  chat_completions: "models",
  responses: "models",
  anthropic_messages: "v1/models",
  image: "models",
}

const BASE_URL_PLACEHOLDER = "https://api.example.com/v1"

let uidCounter = 0

function pairsFrom(obj: Record<string, string> | undefined): KvPair[] {
  return Object.entries(obj ?? {}).map(([key, value]) => ({ key, value }))
}

function jsonPairsFrom(obj: Record<string, Record<string, any>> | undefined): KvPair[] {
  return Object.entries(obj ?? {}).map(([key, value]) => ({
    key,
    value: JSON.stringify(value),
  }))
}

function draftFrom(kind: ApiKind, cfg?: ApiConfig): SectionDraft {
  return {
    uid: ++uidCounter,
    kind,
    base_url: cfg?.base_url ?? "",
    models_endpoint: cfg?.models_endpoint ?? "",
    headers: pairsFrom(cfg?.headers),
    body_overrides: Object.entries(cfg?.body_overrides ?? {}).map(([key, value]) => ({
      key,
      value: typeof value === "string" ? value : JSON.stringify(value),
    })),
    strip_fields: [...(cfg?.strip_fields ?? [])],
    model_overrides: jsonPairsFrom(cfg?.model_overrides),
    role_mappings: pairsFrom(cfg?.role_mappings),
    anthropic_cache_control:
      cfg?.anthropic_cache_control === undefined
        ? "inherit"
        : cfg.anthropic_cache_control
          ? "enable"
          : "disable",
    timeout_secs: cfg?.timeout_secs !== undefined ? String(cfg.timeout_secs) : "",
    chunk_timeout_secs: cfg?.chunk_timeout_secs !== undefined ? String(cfg.chunk_timeout_secs) : "",
    ttfb_timeout_secs: cfg?.ttfb_timeout_secs !== undefined ? String(cfg.ttfb_timeout_secs) : "",
    advancedOpen: false,
  }
}

const sections = ref<SectionDraft[]>(
  API_KIND_ORDER.filter((kind) => props.modelValue[kind]).map((kind) =>
    draftFrom(kind, props.modelValue[kind]),
  ),
)
const fallbackValue = ref<ApiKind | null>(props.fallback ?? null)

function kvToObject(pairs: KvPair[]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const { key, value } of pairs) {
    if (key.trim() && value.trim()) out[key.trim()] = value.trim()
  }
  return out
}

function kvToJson(pairs: KvPair[]): Record<string, any> {
  const out: Record<string, any> = {}
  for (const { key, value } of pairs) {
    if (!key.trim() || !value.trim()) continue
    try {
      out[key.trim()] = JSON.parse(value.trim())
    } catch {
      out[key.trim()] = value.trim()
    }
  }
  return out
}

function parseOptionalNumber(raw: string): number | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  const n = Number(trimmed)
  return Number.isFinite(n) && n >= 0 ? n : undefined
}

function sectionJsonError(d: SectionDraft): string | null {
  for (const { key, value } of d.model_overrides) {
    if (!key.trim() || !value.trim()) continue
    try {
      const parsed = JSON.parse(value.trim())
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        return `Model override "${key.trim()}": value must be a JSON object`
      }
    } catch {
      return `Model override "${key.trim()}": invalid JSON value`
    }
  }
  return null
}

function sectionToConfig(d: SectionDraft): ApiConfig {
  const cfg: ApiConfig = { base_url: d.base_url.trim() }
  const modelsEndpoint = d.models_endpoint.trim()
  if (modelsEndpoint) cfg.models_endpoint = modelsEndpoint
  const headers = kvToObject(d.headers)
  if (Object.keys(headers).length > 0) cfg.headers = headers
  const bodyOverrides = kvToJson(d.body_overrides)
  if (Object.keys(bodyOverrides).length > 0) cfg.body_overrides = bodyOverrides
  const stripFields = d.strip_fields.filter((f) => f.trim()).map((f) => f.trim())
  if (stripFields.length > 0) cfg.strip_fields = stripFields
  const modelOverrides: Record<string, Record<string, any>> = {}
  for (const { key, value } of d.model_overrides) {
    if (!key.trim() || !value.trim()) continue
    try {
      const parsed = JSON.parse(value.trim())
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
        modelOverrides[key.trim()] = parsed
      }
    } catch {
      // Surfaced by sectionJsonError / validate() before submit.
    }
  }
  if (Object.keys(modelOverrides).length > 0) cfg.model_overrides = modelOverrides
  const roleMappings = kvToObject(d.role_mappings)
  if (Object.keys(roleMappings).length > 0) cfg.role_mappings = roleMappings
  if (d.anthropic_cache_control !== "inherit")
    cfg.anthropic_cache_control = d.anthropic_cache_control === "enable"
  const timeout = parseOptionalNumber(d.timeout_secs)
  if (timeout !== undefined) cfg.timeout_secs = timeout
  const chunkTimeout = parseOptionalNumber(d.chunk_timeout_secs)
  if (chunkTimeout !== undefined) cfg.chunk_timeout_secs = chunkTimeout
  const ttfbTimeout = parseOptionalNumber(d.ttfb_timeout_secs)
  if (ttfbTimeout !== undefined) cfg.ttfb_timeout_secs = ttfbTimeout
  return cfg
}

function emitAll() {
  const apis = {} as Record<ApiKind, ApiConfig>
  for (const d of [...sections.value].sort(
    (a, b) => API_KIND_ORDER.indexOf(a.kind) - API_KIND_ORDER.indexOf(b.kind),
  )) {
    apis[d.kind] = sectionToConfig(d)
  }
  emit("update:modelValue", apis)

  const protocols = sections.value.map((d) => d.kind).filter((k) => PROTOCOL_KINDS.has(k))
  if (protocols.length <= 1) {
    if (fallbackValue.value !== null) {
      fallbackValue.value = null
      emit("update:fallback", null)
    }
  } else if (fallbackValue.value && !protocols.includes(fallbackValue.value)) {
    fallbackValue.value = null
    emit("update:fallback", null)
  }
}

const availableKinds = computed(() =>
  API_KIND_ORDER.filter((kind) => !sections.value.some((d) => d.kind === kind)),
)

const protocolKeys = computed(() =>
  sections.value.map((d) => d.kind).filter((k) => PROTOCOL_KINDS.has(k)),
)

const filledBaseUrls = computed<Partial<Record<ProtocolApiKind, string>>>(() => {
  const out: Partial<Record<ProtocolApiKind, string>> = {}
  for (const d of sections.value) {
    if (PROTOCOL_KINDS.has(d.kind) && d.base_url.trim()) {
      out[d.kind as ProtocolApiKind] = d.base_url.trim()
    }
  }
  return out
})

const sectionHeaders = computed<Partial<Record<ProtocolApiKind, Record<string, string>>>>(() => {
  const out: Partial<Record<ProtocolApiKind, Record<string, string>>> = {}
  for (const d of sections.value) {
    if (PROTOCOL_KINDS.has(d.kind)) {
      out[d.kind as ProtocolApiKind] = kvToObject(d.headers)
    }
  }
  return out
})

function onProbeBackfill(items: { protocol: ProtocolApiKind; base_url: string }[]) {
  for (const { protocol, base_url } of items) {
    const existing = sections.value.find((d) => d.kind === protocol)
    if (existing) {
      if (existing.base_url.trim() !== base_url) {
        updateSection(existing.uid, { base_url })
      }
    } else {
      sections.value.push(draftFrom(protocol, { base_url }))
    }
  }
  emitAll()
}

const newKind = ref<ApiKind | "">("")

function addSection() {
  const kind = newKind.value
  if (!kind) return
  sections.value.push(draftFrom(kind))
  newKind.value = ""
  emitAll()
}

function removeSection(uid: number) {
  sections.value = sections.value.filter((d) => d.uid !== uid)
  emitAll()
}

function onFallbackChange(v: unknown) {
  fallbackValue.value = (v as ApiKind) ?? null
  emit("update:fallback", fallbackValue.value)
}

function updateSection(uid: number, patch: Partial<SectionDraft>) {
  const idx = sections.value.findIndex((d) => d.uid === uid)
  if (idx === -1) return
  sections.value[idx] = { ...sections.value[idx], ...patch }
  emitAll()
}

function toggleAdvanced(uid: number) {
  const idx = sections.value.findIndex((d) => d.uid === uid)
  if (idx === -1) return
  sections.value[idx].advancedOpen = !sections.value[idx].advancedOpen
}

function stripFieldsText(d: SectionDraft): string {
  return d.strip_fields.join("\n")
}

function onStripFieldsChange(d: SectionDraft, raw: string) {
  updateSection(d.uid, { strip_fields: raw.split("\n") })
}

function numericValue(raw: string): string | undefined {
  return raw.trim() ? raw : undefined
}

function validate(): string | null {
  if (sections.value.length === 0) {
    return "At least one API section is required (e.g. chat_completions)"
  }
  for (const d of sections.value) {
    if (!d.base_url.trim()) {
      return `API section "${API_KIND_LABELS[d.kind]}": base_url is required`
    }
    const jsonError = sectionJsonError(d)
    if (jsonError) return `API section "${API_KIND_LABELS[d.kind]}": ${jsonError}`
    if (d.timeout_secs.trim() && parseOptionalNumber(d.timeout_secs) === undefined) {
      return `API section "${API_KIND_LABELS[d.kind]}": timeout must be a non-negative number`
    }
    if (d.chunk_timeout_secs.trim() && parseOptionalNumber(d.chunk_timeout_secs) === undefined) {
      return `API section "${API_KIND_LABELS[d.kind]}": chunk timeout must be a non-negative number`
    }
    if (d.ttfb_timeout_secs.trim() && parseOptionalNumber(d.ttfb_timeout_secs) === undefined) {
      return `API section "${API_KIND_LABELS[d.kind]}": TTFB timeout must be a non-negative number`
    }
  }
  if (protocolKeys.value.length > 1 && !fallbackValue.value) {
    return "Fallback protocol is required when multiple protocol sections are declared"
  }
  return null
}

defineExpose({ validate })
</script>

<template>
  <div class="space-y-4">
    <div class="space-y-2">
      <FieldLabel>API Sections *</FieldLabel>

      <div v-for="d in sections" :key="d.uid" class="space-y-3 rounded-md border p-3">
        <div class="flex items-center gap-2">
          <Badge variant="outline" class="font-mono text-xs">
            {{ API_KIND_LABELS[d.kind] }}
          </Badge>
          <span class="text-muted-foreground truncate font-mono text-xs">{{ d.base_url }}</span>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            class="ml-auto"
            aria-label="Remove API section"
            @click="removeSection(d.uid)"
          >
            <X class="h-3.5 w-3.5" />
          </Button>
        </div>

        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div class="space-y-1">
            <FieldLabel :for="`base-url-${d.uid}`">Base URL *</FieldLabel>
            <Input
              :id="`base-url-${d.uid}`"
              :model-value="d.base_url"
              :placeholder="BASE_URL_PLACEHOLDER"
              class="font-mono text-sm"
              @input="updateSection(d.uid, { base_url: ($event.target as HTMLInputElement).value })"
            />
          </div>
          <div class="space-y-1">
            <FieldLabel :for="`models-endpoint-${d.uid}`">Models Endpoint</FieldLabel>
            <Input
              :id="`models-endpoint-${d.uid}`"
              :model-value="d.models_endpoint"
              :placeholder="DEFAULT_MODELS_ENDPOINT_HINTS[d.kind]"
              class="font-mono text-sm"
              @input="
                updateSection(d.uid, { models_endpoint: ($event.target as HTMLInputElement).value })
              "
            />
          </div>
        </div>

        <Button type="button" variant="ghost" size="sm" class="px-2" @click="toggleAdvanced(d.uid)">
          <ChevronDown v-if="d.advancedOpen" class="h-3.5 w-3.5" />
          <ChevronRight v-else class="h-3.5 w-3.5" />
          Advanced
        </Button>

        <div v-if="d.advancedOpen" class="space-y-4">
          <KvEditor
            :model-value="d.headers"
            title="Headers"
            key-placeholder="Header-Name"
            value-placeholder="Value"
            @update:model-value="(v) => updateSection(d.uid, { headers: v as KvPair[] })"
          />

          <Separator />

          <KvEditor
            :model-value="d.body_overrides"
            title="Body Overrides"
            key-placeholder="max_tokens"
            value-placeholder="4096"
            @update:model-value="(v) => updateSection(d.uid, { body_overrides: v as KvPair[] })"
          />

          <Separator />

          <div>
            <FieldLabel>Strip Fields</FieldLabel>
            <textarea
              :value="stripFieldsText(d)"
              rows="2"
              class="border-input placeholder:text-muted-foreground focus-visible:ring-ring/50 flex min-h-16 w-full rounded-md border bg-transparent px-3 py-2 text-sm shadow-xs focus-visible:outline-none"
              placeholder="field_name"
              @input="onStripFieldsChange(d, ($event.target as HTMLTextAreaElement).value)"
            />
          </div>

          <Separator />

          <KvEditor
            :model-value="d.role_mappings"
            title="Role Mappings"
            key-placeholder="developer"
            value-placeholder="system"
            @update:model-value="(v) => updateSection(d.uid, { role_mappings: v as KvPair[] })"
          />

          <Separator />

          <div class="space-y-1">
            <KvEditor
              :model-value="d.model_overrides"
              title="Model Overrides"
              key-placeholder="model-name"
              value-placeholder='{"max_tokens": 4096}'
              @update:model-value="(v) => updateSection(d.uid, { model_overrides: v as KvPair[] })"
            />
            <p v-if="sectionJsonError(d)" class="text-destructive text-xs">
              {{ sectionJsonError(d) }}
            </p>
          </div>

          <Separator />

          <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <div class="space-y-1">
              <FieldLabel :for="`cache-ctl-${d.uid}`">Cache Control</FieldLabel>
              <Select
                :model-value="d.anthropic_cache_control"
                @update:model-value="
                  (v) =>
                    updateSection(d.uid, {
                      anthropic_cache_control: v as 'inherit' | 'enable' | 'disable',
                    })
                "
              >
                <SelectTrigger :id="`cache-ctl-${d.uid}`" class="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent class="w-(--reka-select-trigger-width)">
                  <SelectItem value="inherit">Inherit default</SelectItem>
                  <SelectItem value="enable">Enabled</SelectItem>
                  <SelectItem value="disable">Disabled</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div class="space-y-1">
              <FieldLabel :for="`timeout-${d.uid}`">Timeout (secs)</FieldLabel>
              <Input
                :id="`timeout-${d.uid}`"
                type="number"
                :model-value="d.timeout_secs"
                placeholder="Inherit"
                @input="
                  updateSection(d.uid, {
                    timeout_secs: numericValue(($event.target as HTMLInputElement).value) ?? '',
                  })
                "
              />
            </div>
            <div class="space-y-1">
              <FieldLabel :for="`chunk-timeout-${d.uid}`">Chunk Timeout (secs)</FieldLabel>
              <Input
                :id="`chunk-timeout-${d.uid}`"
                type="number"
                :model-value="d.chunk_timeout_secs"
                placeholder="Inherit"
                @input="
                  updateSection(d.uid, {
                    chunk_timeout_secs:
                      numericValue(($event.target as HTMLInputElement).value) ?? '',
                  })
                "
              />
            </div>
            <div class="space-y-1">
              <FieldLabel :for="`ttfb-timeout-${d.uid}`">TTFB Timeout (secs)</FieldLabel>
              <Input
                :id="`ttfb-timeout-${d.uid}`"
                type="number"
                :model-value="d.ttfb_timeout_secs"
                placeholder="Inherit"
                @input="
                  updateSection(d.uid, {
                    ttfb_timeout_secs:
                      numericValue(($event.target as HTMLInputElement).value) ?? '',
                  })
                "
              />
            </div>
          </div>
        </div>
      </div>

      <div v-if="availableKinds.length > 0" class="flex items-center gap-2">
        <Select v-model="newKind">
          <SelectTrigger class="w-56">
            <SelectValue placeholder="Add API section…" />
          </SelectTrigger>
          <SelectContent class="w-(--reka-select-trigger-width)">
            <SelectItem v-for="kind in availableKinds" :key="kind" :value="kind">
              {{ API_KIND_LABELS[kind] }}
            </SelectItem>
          </SelectContent>
        </Select>
        <Button type="button" variant="outline" size="sm" :disabled="!newKind" @click="addSection">
          <Plus class="h-3.5 w-3.5" />
          Add Section
        </Button>
      </div>

      <p v-if="sections.length === 0" class="text-destructive text-xs">
        At least one API section is required.
      </p>
    </div>

    <Separator />
    <ProtocolProbePanel
      :filled-base-urls="filledBaseUrls"
      :section-headers="sectionHeaders"
      :api-keys="apiKeys ?? []"
      :proxy-mode="proxyMode ?? 'inherit'"
      :headers="providerHeaders ?? {}"
      @backfill="onProbeBackfill"
    />

    <div class="space-y-1">
      <FieldLabel>Fallback Protocol</FieldLabel>
      <template v-if="protocolKeys.length === 1">
        <p class="text-muted-foreground text-xs">
          Auto: <code>{{ API_KIND_LABELS[protocolKeys[0]] }}</code>
        </p>
      </template>
      <template v-else-if="protocolKeys.length > 1">
        <Select :model-value="fallbackValue ?? undefined" @update:model-value="onFallbackChange">
          <SelectTrigger class="w-56">
            <SelectValue placeholder="Select fallback protocol *" />
          </SelectTrigger>
          <SelectContent class="w-(--reka-select-trigger-width)">
            <SelectItem v-for="kind in protocolKeys" :key="kind" :value="kind">
              {{ API_KIND_LABELS[kind] }}
            </SelectItem>
          </SelectContent>
        </Select>
      </template>
      <template v-else>
        <p class="text-muted-foreground text-xs">Add a protocol section first.</p>
      </template>
    </div>
  </div>
</template>

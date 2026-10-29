<script setup lang="ts">
import {
  AlertCircle,
  AlertTriangle,
  ChevronRight,
  Info,
  Loader2,
  Plus,
  Save,
  ShieldCheck,
  Trash2,
  Undo2,
} from "@lucide/vue"
import { computed, nextTick, ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group"
import { Separator } from "@/components/ui/separator"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import {
  buildPrivacyPayload,
  createPrivacyDraft,
  getPrivacyChanges,
  validatePrivacyDraft,
  type PrivacyDraft,
} from "@/lib/privacyConfig"
import { useGatewayConfigStore } from "@/stores/gatewayConfig"
import type { PrivacyBuiltins, PrivacyConfigSnapshot, PrivacyMode } from "@/types/api"

const store = useGatewayConfigStore()

/** Last confirmed `[privacy]` snapshot from the gateway (a deep copy, never aliased to the store). */
const saved = ref<PrivacyConfigSnapshot | null>(null)
/** Editable draft; survives failed saves, only rebuilt on first load, undo and a confirmed save. */
const draft = ref<PrivacyDraft | null>(null)
/** Required-field failures, keyed by row id because several rows may share one kind. */
const fieldErrors = ref<{ rowId: string; field: RegexField }[]>([])
/** Raw backend message of the last failed save (never includes the salt: the request is not logged). */
const saveError = ref("")
const localSaving = ref(false)

/** Row-id source for regex rows the user appends (`new-<n>`); saved rows keep helper ids `saved-<i>`. */
let newRowSequence = 0

type RegexField = "kind" | "pattern"

// Labels are thunks so every key stays a literal `t("...")` call (the i18n
// dead-key report matches those) while the tables can still be iterated.
interface BuiltinRow {
  key: keyof PrivacyBuiltins
  label: () => string
  hint: () => string
}

const BUILTINS: readonly BuiltinRow[] = [
  {
    key: "email",
    label: () => t("settings.privacy.builtin.email"),
    hint: () => t("settings.privacy.builtin.email.hint"),
  },
  {
    key: "phone",
    label: () => t("settings.privacy.builtin.phone"),
    hint: () => t("settings.privacy.builtin.phone.hint"),
  },
  {
    key: "id_card_cn",
    label: () => t("settings.privacy.builtin.id_card_cn"),
    hint: () => t("settings.privacy.builtin.id_card_cn.hint"),
  },
  {
    key: "bank_card",
    label: () => t("settings.privacy.builtin.bank_card"),
    hint: () => t("settings.privacy.builtin.bank_card.hint"),
  },
  {
    key: "iban",
    label: () => t("settings.privacy.builtin.iban"),
    hint: () => t("settings.privacy.builtin.iban.hint"),
  },
  {
    key: "api_key",
    label: () => t("settings.privacy.builtin.api_key"),
    hint: () => t("settings.privacy.builtin.api_key.hint"),
  },
  {
    key: "private_key",
    label: () => t("settings.privacy.builtin.private_key"),
    hint: () => t("settings.privacy.builtin.private_key.hint"),
  },
]

interface ModeRow {
  value: PrivacyMode
  label: () => string
  description: () => string
  recommended?: boolean
}

const MODES: readonly ModeRow[] = [
  {
    value: "mask",
    label: () => t("settings.privacy.mode.mask"),
    description: () => t("settings.privacy.mode.maskDescription"),
    recommended: true,
  },
  {
    value: "block",
    label: () => t("settings.privacy.mode.block"),
    description: () => t("settings.privacy.mode.blockDescription"),
  },
  {
    value: "warn",
    label: () => t("settings.privacy.mode.warn"),
    description: () => t("settings.privacy.mode.warnDescription"),
  },
]

const SCOPE_ITEMS: readonly { id: string; text: () => string }[] = [
  { id: "chat", text: () => t("settings.privacy.scopeChat") },
  { id: "media", text: () => t("settings.privacy.scopeMedia") },
  { id: "upstream", text: () => t("settings.privacy.scopeUpstream") },
  { id: "preview", text: () => t("settings.privacy.scopePreview") },
]

/**
 * JSON round-trip clone: the store hands out reactive proxies, which
 * `structuredClone` rejects. The clone is what later diffs are measured against.
 */
function cloneSnapshot(source: PrivacyConfigSnapshot): PrivacyConfigSnapshot {
  return JSON.parse(JSON.stringify(source)) as PrivacyConfigSnapshot
}

/**
 * Build the draft from the first snapshot that actually carries `privacy`, once.
 * Later store updates (a refetch, or another form's save) never overwrite a
 * draft that may hold unsaved edits.
 */
function initFromStore() {
  if (draft.value !== null) return
  const privacy = store.config?.privacy
  if (!privacy) return
  saved.value = cloneSnapshot(privacy)
  draft.value = createPrivacyDraft(saved.value)
}

watch(() => store.config, initFromStore, { immediate: true })

const locked = computed(() => localSaving.value || store.saving)
const loading = computed(
  () => draft.value === null && (store.loading || (store.config === null && store.error === "")),
)
/** The gateway answered, but without a `privacy` section (backend older than this page). */
const unavailable = computed(() => store.config !== null && !store.config.privacy)

const changes = computed(() =>
  draft.value && saved.value ? getPrivacyChanges(draft.value, saved.value) : [],
)
const dirty = computed(() => changes.value.length > 0)

const builtinCount = computed(() =>
  draft.value ? BUILTINS.filter((row) => draft.value!.builtins[row.key]).length : 0,
)
const savedBuiltinCount = computed(() =>
  saved.value ? BUILTINS.filter((row) => saved.value!.builtins[row.key]).length : 0,
)

const savedEnabledLabel = computed(() =>
  saved.value?.enabled ? t("common.enabled") : t("common.disabled"),
)
const savedModeLabel = computed(() => (saved.value ? modeLabel(saved.value.mode) : ""))
const savedRestoreLabel = computed(() =>
  saved.value?.restore ? t("common.enabled") : t("common.disabled"),
)
const savedSaltLabel = computed(() =>
  saved.value?.salt_configured
    ? t("settings.privacy.saltConfigured")
    : t("settings.privacy.saltNotConfigured"),
)
const draftEnabledLabel = computed(() =>
  draft.value?.enabled ? t("common.enabled") : t("common.disabled"),
)
const draftModeLabel = computed(() => (draft.value ? modeLabel(draft.value.mode) : ""))
const saltPending = computed(() => (draft.value?.newSalt ?? "") !== "")
const draftSaltLabel = computed(() =>
  saltPending.value ? t("settings.privacy.saltPendingReplace") : savedSaltLabel.value,
)
const restoreStateLabel = computed(() =>
  draft.value?.restore ? t("common.enabled") : t("common.disabled"),
)

function modeLabel(mode: PrivacyMode): string {
  const row = MODES.find((item) => item.value === mode)
  return row ? row.label() : mode
}

/**
 * Index (0-based) of the regex row the gateway rejected in the last save
 * attempt. The backend names `privacy.custom_regexes[i]`; payload order is the
 * visible row order, so `i` maps straight onto the row list.
 */
const rejectedRegexIndex = computed(() => {
  const match = /privacy\.custom_regexes\[(\d+)\]/.exec(saveError.value)
  if (!match || !draft.value) return null
  const index = Number(match[1])
  if (!Number.isInteger(index) || index < 0 || index >= draft.value.regexes.length) return null
  return index
})

async function retryLoad() {
  await store.fetchConfig()
}

function handleUndo() {
  if (locked.value || !saved.value) return
  draft.value = createPrivacyDraft(saved.value)
  fieldErrors.value = []
  saveError.value = ""
}

function hasFieldError(rowId: string, field: RegexField): boolean {
  return fieldErrors.value.some((error) => error.rowId === rowId && error.field === field)
}

function clearFieldError(rowId: string, field: RegexField) {
  fieldErrors.value = fieldErrors.value.filter(
    (error) => !(error.rowId === rowId && error.field === field),
  )
}

function onRegexFieldInput(row: { id: string; kind: string; pattern: string }, field: RegexField) {
  if (row[field].trim() !== "") clearFieldError(row.id, field)
}

function addRegexRow() {
  if (locked.value || !draft.value) return
  newRowSequence += 1
  draft.value.regexes.push({
    id: `new-${newRowSequence}`,
    kind: "",
    pattern: "",
    case_insensitive: false,
  })
}

function removeRegexRow(rowId: string) {
  if (locked.value || !draft.value) return
  draft.value.regexes = draft.value.regexes.filter((row) => row.id !== rowId)
  fieldErrors.value = fieldErrors.value.filter((error) => error.rowId !== rowId)
  // The backend error names a 0-based row index, so deleting any row shifts the
  // index and the highlight would land on a different rule. Drop the banner
  // instead of pointing at a row we can no longer identify.
  saveError.value = ""
}

function selectMode(value: PrivacyMode) {
  if (locked.value || !draft.value) return
  draft.value.mode = value
}

/** Focus targets for the regex inputs, so a blocked save lands on the first invalid field. */
const regexFieldEls = new Map<string, HTMLInputElement>()

function bindRegexField(rowId: string, field: RegexField, el: unknown) {
  const key = `${rowId}:${field}`
  // `:ref` on an element yields the element, but on a component it yields the
  // public instance, whose root DOM node is `$el`.
  const node = el instanceof HTMLInputElement ? el : (el as { $el?: unknown } | null)?.$el
  if (node instanceof HTMLInputElement) regexFieldEls.set(key, node)
  else regexFieldEls.delete(key)
}

async function handleSave() {
  if (!draft.value || !saved.value || locked.value) return
  saveError.value = ""
  const validation = validatePrivacyDraft(draft.value)
  fieldErrors.value = validation.map(({ rowId, field }) => ({ rowId, field }))
  if (validation.length > 0) {
    await nextTick()
    const first = validation[0]
    regexFieldEls.get(`${first.rowId}:${first.field}`)?.focus()
    return
  }

  // Freeze the request body before awaiting: nothing edited afterwards (or by a
  // re-render) may leak into what is being sent. `salt` stays out while blank.
  const pending = buildPrivacyPayload(draft.value, saved.value)
  localSaving.value = true
  try {
    const snapshot = await store.saveRuntimeConfig({ privacy: pending })
    const confirmed = snapshot?.privacy ?? store.config?.privacy
    if (!confirmed) {
      saveError.value = t("settings.privacy.saveFailed", {
        message: t("settings.privacy.unavailable"),
      })
      return
    }
    saved.value = cloneSnapshot(confirmed)
    // Rebuilding from the server response also clears the salt input.
    draft.value = createPrivacyDraft(saved.value)
    fieldErrors.value = []
    toast.success(t("settings.toast.privacySaved"))
  } catch (e: any) {
    // Draft (including a pending salt) is kept on purpose: we cannot know whether
    // the gateway applied the change, so nothing is discarded or over-claimed.
    saveError.value = t("settings.privacy.saveFailed", { message: store.error || String(e) })
  } finally {
    localSaving.value = false
  }
}
</script>

<template>
  <Card v-if="!draft">
    <CardHeader>
      <CardTitle>{{ $t("settings.privacy.title") }}</CardTitle>
    </CardHeader>
    <CardContent class="space-y-3">
      <div v-if="loading" class="text-muted-foreground flex items-center gap-2 text-sm">
        <Loader2 class="h-4 w-4 animate-spin" />
        {{ $t("settings.privacy.loading") }}
      </div>
      <template v-else>
        <div
          class="bg-destructive/10 text-destructive flex items-start gap-2 rounded-md px-3 py-2 text-xs"
        >
          <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span class="min-w-0 break-words">
            {{ unavailable ? $t("settings.privacy.unavailable") : $t("settings.privacy.loadError")
            }}<template v-if="!unavailable && store.error"> {{ store.error }}</template>
          </span>
        </div>
        <Button variant="outline" size="sm" :disabled="store.loading" @click="retryLoad">
          <Loader2 v-if="store.loading" class="h-3.5 w-3.5 animate-spin" />
          {{ $t("common.retry") }}
        </Button>
      </template>
    </CardContent>
  </Card>

  <div v-else class="space-y-4" :aria-busy="locked">
    <div class="min-w-0 space-y-1">
      <h2 class="flex items-center gap-2 text-base font-semibold">
        <ShieldCheck class="h-4 w-4" />
        {{ $t("settings.privacy.title") }}
      </h2>
      <p class="text-muted-foreground max-w-3xl text-xs">
        {{ $t("settings.privacy.description") }}
      </p>
    </div>

    <!-- ── Saved vs draft summary ────────────────────────────────── -->
    <Card>
      <CardHeader>
        <div class="min-w-0 space-y-1">
          <CardTitle>{{ $t("settings.privacy.statusTitle") }}</CardTitle>
          <p class="text-muted-foreground max-w-3xl text-xs">
            {{ $t("settings.privacy.statusDescription") }}
          </p>
        </div>
      </CardHeader>
      <CardContent>
        <div class="grid gap-4 sm:grid-cols-2 sm:gap-6">
          <div class="min-w-0 space-y-2">
            <div class="flex flex-wrap items-center justify-between gap-2">
              <span class="text-sm font-medium">{{ $t("settings.privacy.savedTitle") }}</span>
            </div>
            <div class="flex flex-wrap gap-1.5">
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("common.status") }}</span>
                <span>{{ savedEnabledLabel }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("settings.privacy.summaryMode") }}</span>
                <span>{{ savedModeLabel }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">
                  {{ $t("settings.privacy.summaryBuiltins") }}
                </span>
                <span>{{ savedBuiltinCount }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">
                  {{ $t("settings.privacy.summaryRestore") }}
                </span>
                <span>{{ savedRestoreLabel }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("settings.privacy.summarySalt") }}</span>
                <span>{{ savedSaltLabel }}</span>
              </Badge>
            </div>
          </div>
          <div class="min-w-0 space-y-2 sm:border-l sm:pl-6">
            <div class="flex flex-wrap items-center justify-between gap-2">
              <span class="text-sm font-medium">{{ $t("settings.privacy.draftTitle") }}</span>
              <Badge
                variant="outline"
                class="shrink-0 text-[10px]"
                :class="dirty ? 'border-warning/50 bg-warning/20' : 'text-muted-foreground'"
              >
                {{ dirty ? $t("settings.privacy.draftDirty") : $t("settings.privacy.draftSynced") }}
              </Badge>
            </div>
            <div class="flex flex-wrap gap-1.5">
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("common.status") }}</span>
                <span>{{ draftEnabledLabel }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("settings.privacy.summaryMode") }}</span>
                <span>{{ draftModeLabel }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">
                  {{ $t("settings.privacy.summaryBuiltins") }}
                </span>
                <span>{{ builtinCount }}</span>
              </Badge>
              <Badge variant="outline" class="gap-1 font-normal">
                <span class="text-muted-foreground">{{ $t("settings.privacy.summarySalt") }}</span>
                <span>{{ draftSaltLabel }}</span>
              </Badge>
            </div>
          </div>
        </div>
      </CardContent>
    </Card>

    <!-- ── Basics: master switch, mode, restore ──────────────────── -->
    <Card>
      <CardHeader>
        <div class="min-w-0 space-y-1">
          <CardTitle>{{ $t("settings.privacy.basicTitle") }}</CardTitle>
          <p class="text-muted-foreground max-w-3xl text-xs">
            {{ $t("settings.privacy.basicDescription") }}
          </p>
        </div>
      </CardHeader>
      <CardContent class="space-y-4">
        <div class="flex items-center justify-between gap-4">
          <div class="flex min-w-0 items-center gap-1.5">
            <Label for="privacy-enabled" class="text-sm font-medium">
              {{ $t("settings.privacy.enabledLabel") }}
            </Label>
            <Tooltip>
              <TooltipTrigger as-child>
                <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
              </TooltipTrigger>
              <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
                {{ $t("settings.privacy.enabledHint") }}
              </TooltipContent>
            </Tooltip>
          </div>
          <Switch id="privacy-enabled" v-model="draft.enabled" :disabled="locked" />
        </div>

        <p
          v-if="!draft.enabled"
          class="bg-muted/60 text-muted-foreground border-primary/40 rounded-md border-l-2 px-3 py-2 text-xs"
        >
          {{ $t("settings.privacy.disabledNote") }}
        </p>

        <Separator />

        <div class="space-y-2">
          <div class="min-w-0">
            <span class="text-sm font-medium">{{ $t("settings.privacy.modeTitle") }}</span>
            <p class="text-muted-foreground mt-0.5 max-w-3xl text-xs">
              {{ $t("settings.privacy.modeDescription") }}
            </p>
          </div>
          <RadioGroup
            v-model="draft.mode"
            :disabled="locked"
            :aria-label="$t('settings.privacy.modeTitle')"
            class="grid gap-2.5 sm:grid-cols-3"
          >
            <div
              v-for="mode in MODES"
              :key="mode.value"
              class="min-w-0 space-y-1.5 rounded-lg border p-3 transition-colors"
              :class="
                draft.mode === mode.value
                  ? 'border-primary bg-primary/5 ring-primary ring-1'
                  : 'border-border hover:border-primary/45'
              "
              @click="selectMode(mode.value)"
            >
              <div class="flex flex-wrap items-center gap-1.5">
                <RadioGroupItem
                  :id="`privacy-mode-${mode.value}`"
                  :value="mode.value"
                  class="mt-0.5"
                />
                <Label :for="`privacy-mode-${mode.value}`" class="text-sm font-medium break-words">
                  {{ mode.label() }}
                </Label>
                <Badge
                  v-if="mode.recommended"
                  variant="outline"
                  class="border-primary/50 bg-primary/20 text-[10px]"
                >
                  {{ $t("settings.privacy.mode.maskRecommended") }}
                </Badge>
              </div>
              <p class="text-muted-foreground text-xs break-words">
                {{ mode.description() }}
              </p>
              <p
                v-if="mode.value === 'warn'"
                class="border-warning/50 bg-warning/20 text-warning-foreground flex items-start gap-1.5 rounded-md border px-2 py-1.5 text-xs break-words"
              >
                <AlertTriangle class="mt-px h-3.5 w-3.5 shrink-0" />
                <span class="min-w-0">{{ $t("settings.privacy.mode.warnWarning") }}</span>
              </p>
            </div>
          </RadioGroup>
          <p
            v-if="draft.mode === 'block'"
            class="bg-muted/60 text-muted-foreground rounded-md px-3 py-2 text-xs break-words"
          >
            {{ $t("settings.privacy.mode.blockWarning") }}
          </p>
          <p
            v-if="draft.mode === 'warn'"
            class="bg-muted/60 text-muted-foreground rounded-md px-3 py-2 text-xs break-words"
          >
            {{ $t("settings.privacy.mode.warnLog") }}
          </p>
        </div>

        <div v-if="draft.mode === 'mask'" class="flex items-center justify-between gap-4">
          <div class="flex min-w-0 items-center gap-1.5">
            <Label for="privacy-restore" class="text-sm font-medium">
              {{ $t("settings.privacy.restoreLabel") }}
            </Label>
            <Tooltip>
              <TooltipTrigger as-child>
                <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
              </TooltipTrigger>
              <TooltipContent side="top" class="max-w-xs font-normal break-words whitespace-normal">
                {{ $t("settings.privacy.restoreHint") }}
              </TooltipContent>
            </Tooltip>
          </div>
          <Switch id="privacy-restore" v-model="draft.restore" :disabled="locked" />
        </div>
        <p
          v-else
          class="bg-muted/60 text-muted-foreground rounded-md px-3 py-2 text-xs break-words"
        >
          {{
            $t("settings.privacy.restoreMaskOnly", {
              state: restoreStateLabel,
            })
          }}
        </p>
      </CardContent>
    </Card>

    <!-- ── Built-in detectors ───────────────────────────────────── -->
    <Card>
      <CardHeader>
        <div class="flex flex-wrap items-start justify-between gap-3">
          <div class="min-w-0 space-y-1">
            <CardTitle>{{ $t("settings.privacy.builtinsTitle") }}</CardTitle>
            <p class="text-muted-foreground max-w-3xl text-xs">
              {{ $t("settings.privacy.builtinsDescription") }}
            </p>
          </div>
          <Badge variant="secondary" class="shrink-0 text-[10px]">
            {{
              $t("settings.privacy.builtinsCount", {
                enabled: builtinCount,
                total: BUILTINS.length,
              })
            }}
          </Badge>
        </div>
      </CardHeader>
      <CardContent>
        <div class="grid gap-x-6 sm:grid-cols-2">
          <div
            v-for="builtin in BUILTINS"
            :key="builtin.key"
            class="flex items-center justify-between gap-4 py-2.5"
          >
            <div class="flex min-w-0 flex-wrap items-center gap-1.5">
              <Label :for="`privacy-builtin-${builtin.key}`" class="text-sm font-medium">
                {{ builtin.label() }}
              </Label>
              <code class="text-muted-foreground font-mono text-[10px]">{{ builtin.key }}</code>
              <Tooltip>
                <TooltipTrigger as-child>
                  <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
                </TooltipTrigger>
                <TooltipContent
                  side="top"
                  class="max-w-xs font-normal break-words whitespace-normal"
                >
                  {{ builtin.hint() }}
                </TooltipContent>
              </Tooltip>
            </div>
            <Switch
              :id="`privacy-builtin-${builtin.key}`"
              v-model="draft.builtins[builtin.key]"
              :disabled="locked"
            />
          </div>
        </div>
      </CardContent>
    </Card>

    <!-- ── Custom rules and exceptions (collapsed) ──────────────── -->
    <details class="bg-card rounded-xl border [&[open]_.privacy-chevron]:rotate-90">
      <summary
        class="flex cursor-pointer list-none items-start justify-between gap-3 p-4 [&::-webkit-details-marker]:hidden"
      >
        <div class="min-w-0 space-y-1">
          <h3 class="flex flex-wrap items-center gap-2 text-sm font-semibold">
            {{ $t("settings.privacy.customSectionTitle") }}
            <Badge variant="outline" class="text-[10px]">
              {{ $t("settings.privacy.customSectionBadge") }}
            </Badge>
          </h3>
          <p class="text-muted-foreground max-w-3xl text-xs">
            {{ $t("settings.privacy.customSectionDescription") }}
          </p>
        </div>
        <ChevronRight
          class="privacy-chevron text-muted-foreground mt-0.5 h-4 w-4 shrink-0 transition-transform"
        />
      </summary>
      <div class="space-y-5 border-t px-4 pt-4 pb-4">
        <div class="space-y-1.5">
          <Label for="privacy-custom-words" class="text-sm font-medium">
            {{ $t("settings.privacy.customWordsTitle") }}
          </Label>
          <Textarea
            id="privacy-custom-words"
            v-model="draft.customWordsText"
            :rows="3"
            spellcheck="false"
            :disabled="locked"
          />
          <p class="text-muted-foreground text-xs break-words">
            {{ $t("settings.privacy.customWordsHint") }}
          </p>
        </div>

        <div class="space-y-1.5">
          <Label for="privacy-allowlist" class="text-sm font-medium">
            {{ $t("settings.privacy.allowlistTitle") }}
          </Label>
          <Textarea
            id="privacy-allowlist"
            v-model="draft.allowlistText"
            :rows="3"
            spellcheck="false"
            :disabled="locked"
          />
          <p class="text-muted-foreground text-xs break-words">
            {{ $t("settings.privacy.allowlistHint") }}
          </p>
        </div>

        <div class="space-y-2">
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div class="min-w-0 space-y-1">
              <span class="text-sm font-medium">{{ $t("settings.privacy.regexTitle") }}</span>
              <p class="text-muted-foreground max-w-3xl text-xs">
                {{ $t("settings.privacy.regexDescription") }}
              </p>
            </div>
            <Button
              variant="outline"
              size="sm"
              class="shrink-0"
              :disabled="locked"
              @click="addRegexRow"
            >
              <Plus class="h-3.5 w-3.5" />
              {{ $t("settings.privacy.regexAdd") }}
            </Button>
          </div>
          <p class="text-muted-foreground text-xs break-words">
            {{ $t("settings.privacy.regexKindHint") }}
          </p>

          <p v-if="draft.regexes.length === 0" class="text-muted-foreground text-xs">
            {{ $t("settings.privacy.regexEmpty") }}
          </p>

          <div
            v-for="(row, index) in draft.regexes"
            :key="row.id"
            class="space-y-2 rounded-md border p-2.5"
            :class="rejectedRegexIndex === index ? 'border-warning/60' : 'border-border'"
          >
            <div class="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.7fr)_auto_auto]">
              <div class="min-w-0 space-y-1">
                <Label :for="`privacy-regex-kind-${row.id}`" class="text-xs font-medium">
                  {{ $t("settings.privacy.regexKindLabel") }}
                </Label>
                <Input
                  :id="`privacy-regex-kind-${row.id}`"
                  :ref="(el) => bindRegexField(row.id, 'kind', el)"
                  :model-value="row.kind"
                  :placeholder="$t('settings.privacy.regexKindPlaceholder')"
                  :disabled="locked"
                  :aria-invalid="hasFieldError(row.id, 'kind') || undefined"
                  :aria-describedby="
                    hasFieldError(row.id, 'kind') ? `privacy-regex-kind-error-${row.id}` : undefined
                  "
                  @update:model-value="
                    (value) => {
                      row.kind = String(value)
                      onRegexFieldInput(row, 'kind')
                    }
                  "
                />
                <p
                  v-if="hasFieldError(row.id, 'kind')"
                  :id="`privacy-regex-kind-error-${row.id}`"
                  class="text-destructive text-xs"
                >
                  {{ $t("settings.privacy.regexKindRequired") }}
                </p>
              </div>
              <div class="min-w-0 space-y-1">
                <Label :for="`privacy-regex-pattern-${row.id}`" class="text-xs font-medium">
                  {{ $t("settings.privacy.regexPatternLabel") }}
                </Label>
                <Input
                  :id="`privacy-regex-pattern-${row.id}`"
                  :ref="(el) => bindRegexField(row.id, 'pattern', el)"
                  :model-value="row.pattern"
                  :placeholder="$t('settings.privacy.regexPatternPlaceholder')"
                  class="font-mono"
                  :disabled="locked"
                  :aria-invalid="hasFieldError(row.id, 'pattern') || undefined"
                  :aria-describedby="
                    hasFieldError(row.id, 'pattern')
                      ? `privacy-regex-pattern-error-${row.id}`
                      : undefined
                  "
                  @update:model-value="
                    (value) => {
                      row.pattern = String(value)
                      onRegexFieldInput(row, 'pattern')
                    }
                  "
                />
                <p
                  v-if="hasFieldError(row.id, 'pattern')"
                  :id="`privacy-regex-pattern-error-${row.id}`"
                  class="text-destructive text-xs"
                >
                  {{ $t("settings.privacy.regexPatternRequired") }}
                </p>
              </div>
              <div class="flex items-center gap-1.5 pt-1 sm:pt-5">
                <Switch
                  :id="`privacy-regex-ci-${row.id}`"
                  v-model="row.case_insensitive"
                  :disabled="locked"
                />
                <Label :for="`privacy-regex-ci-${row.id}`" class="text-xs whitespace-nowrap">
                  {{ $t("settings.privacy.regexCaseInsensitive") }}
                </Label>
              </div>
              <div class="flex items-center sm:pt-4">
                <Button
                  variant="ghost"
                  size="icon-sm"
                  :disabled="locked"
                  :aria-label="$t('settings.privacy.regexRemove')"
                  :title="$t('settings.privacy.regexRemove')"
                  @click="removeRegexRow(row.id)"
                >
                  <Trash2 class="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <p
              v-if="rejectedRegexIndex === index"
              class="border-warning/50 bg-warning/15 text-warning-foreground flex items-start gap-1.5 rounded-md border px-2 py-1.5 text-xs break-words"
            >
              <AlertTriangle class="mt-px h-3.5 w-3.5 shrink-0" />
              <span class="min-w-0">
                {{
                  $t("settings.privacy.regexIndexError", {
                    index: index + 1,
                    message: saveError,
                  })
                }}
              </span>
            </p>
          </div>

          <p class="text-muted-foreground text-xs break-words">
            {{ $t("settings.privacy.regexDialectNote") }}
          </p>
        </div>
      </div>
    </details>

    <!-- ── Advanced: salt (collapsed) ───────────────────────────── -->
    <details class="bg-card rounded-xl border [&[open]_.privacy-chevron]:rotate-90">
      <summary
        class="flex cursor-pointer list-none items-start justify-between gap-3 p-4 [&::-webkit-details-marker]:hidden"
      >
        <div class="min-w-0 space-y-1">
          <h3 class="text-sm font-semibold">{{ $t("settings.privacy.advancedTitle") }}</h3>
          <p class="text-muted-foreground max-w-3xl text-xs">
            {{ $t("settings.privacy.advancedDescription") }}
          </p>
        </div>
        <ChevronRight
          class="privacy-chevron text-muted-foreground mt-0.5 h-4 w-4 shrink-0 transition-transform"
        />
      </summary>
      <div class="space-y-3 border-t px-4 pt-4 pb-4">
        <div class="flex flex-wrap items-start justify-between gap-2">
          <div class="min-w-0 space-y-1">
            <span class="text-sm font-medium">{{ $t("settings.privacy.saltTitle") }}</span>
            <p class="text-muted-foreground max-w-3xl text-xs break-words">
              {{ $t("settings.privacy.saltHint") }}
            </p>
          </div>
          <div class="flex shrink-0 flex-wrap items-center gap-1.5">
            <Badge variant="outline" class="text-[10px]">{{ savedSaltLabel }}</Badge>
            <Badge
              v-if="saltPending"
              variant="outline"
              class="border-primary/50 bg-primary/20 text-[10px]"
            >
              {{ $t("settings.privacy.saltPendingReplace") }}
            </Badge>
          </div>
        </div>
        <div class="max-w-sm space-y-1.5">
          <Label for="privacy-salt" class="text-sm font-medium">
            {{ $t("settings.privacy.saltNewLabel") }}
          </Label>
          <Input
            id="privacy-salt"
            v-model="draft.newSalt"
            type="password"
            autocomplete="new-password"
            spellcheck="false"
            class="font-mono"
            :placeholder="$t('settings.privacy.saltNewPlaceholder')"
            :disabled="locked"
          />
          <p class="text-muted-foreground text-xs break-words">
            {{ $t("settings.privacy.saltNewHint") }}
          </p>
        </div>
      </div>
    </details>

    <!-- ── Coverage limits ─────────────────────────────────────── -->
    <Card>
      <CardHeader>
        <div class="min-w-0 space-y-1">
          <CardTitle>{{ $t("settings.privacy.scopeTitle") }}</CardTitle>
          <p class="text-muted-foreground max-w-3xl text-xs">
            {{ $t("settings.privacy.scopeDescription") }}
          </p>
        </div>
      </CardHeader>
      <CardContent>
        <ul class="space-y-2">
          <li v-for="item in SCOPE_ITEMS" :key="item.id" class="flex items-start gap-2 text-xs">
            <span class="bg-primary mt-1.5 h-1.5 w-1.5 shrink-0 rounded-full" aria-hidden="true" />
            <span class="min-w-0 break-words">{{ item.text() }}</span>
          </li>
        </ul>
      </CardContent>
    </Card>

    <div
      v-if="saveError"
      class="border-warning/50 bg-warning/15 text-warning-foreground flex items-start gap-2 rounded-md border px-3 py-2 text-xs"
    >
      <AlertTriangle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
      <div class="min-w-0 space-y-1">
        <p class="break-words">{{ saveError }}</p>
        <p class="text-muted-foreground">{{ $t("settings.privacy.saveUnconfirmedHint") }}</p>
      </div>
    </div>

    <div
      class="bg-background/95 sticky bottom-0 z-10 flex flex-wrap items-center justify-between gap-3 border-t py-3 backdrop-blur"
    >
      <div class="flex min-w-0 flex-wrap items-center gap-2">
        <Badge
          variant="outline"
          class="shrink-0 text-[10px]"
          :class="dirty ? 'border-warning/50 bg-warning/20' : 'text-muted-foreground'"
        >
          {{ dirty ? $t("settings.privacy.draftDirty") : $t("settings.privacy.draftSynced") }}
        </Badge>
        <span class="text-muted-foreground text-xs break-words">
          {{ locked ? $t("settings.privacy.savingLocked") : $t("settings.privacy.footerHint") }}
        </span>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <Button variant="outline" :disabled="!dirty || locked" @click="handleUndo">
          <Undo2 class="h-3.5 w-3.5" />
          {{ $t("settings.privacy.undo") }}
        </Button>
        <Button :disabled="!dirty || locked" :aria-busy="locked" @click="handleSave">
          <Loader2 v-if="locked" class="h-3.5 w-3.5 animate-spin" />
          <Save v-else class="h-3.5 w-3.5" />
          {{ locked ? $t("common.saving") : $t("common.save") }}
        </Button>
      </div>
    </div>
  </div>
</template>

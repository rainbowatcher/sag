<script setup lang="ts">
import { KeyRound, Loader2, TriangleAlert, X } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { errorForModel, isPinActive, shortSessionKey, sortPins } from "@/lib/sessionAffinity"
import { formatDuration, getErrorMessage } from "@/lib/utils"
import { useSessionAffinityStore } from "@/stores/sessionAffinity"
import type { SessionAffinityEntry } from "@/types/api"

/**
 * Session-affinity pins of one `session_sticky` route: lists the live pins of
 * the route's request-model space and clears either a single session
 * (`DELETE { key }`) or the whole space (`DELETE { model }`).
 *
 * Hovering the key icon previews the same pin list in a tooltip (no click
 * needed); clicking still opens the popover that carries the clear actions.
 */
const props = defineProps<{ model: string }>()
const emit = defineEmits<{ changed: [] }>()

const store = useSessionAffinityStore()

const open = ref(false)
const clearingKey = ref<string | null>(null)
const clearingModel = ref(false)
/** Two-step confirm for the "clear this model" destructive action. */
const confirmClearModel = ref(false)

// The store is shared by every row's popover; show only the space this
// popover was opened for so a refresh from another row cannot leak in.
const rows = computed(() => store.entries.filter((e) => e.model === props.model))
/** Live pins first, expired leftovers last — shared by the tooltip and the popover. */
const sortedRows = computed(() => sortPins(rows.value))
/**
 * The failure of *this* row's last fetch. The store is shared, so another
 * row's error must not replace this row's list with its message.
 */
const refreshError = computed(() => errorForModel(store.error, props.model))

// Hovering the icon fetches too, but only when the shared store is stale, so
// sweeping the pointer across rows cannot fire a request per pixel.
const HOVER_REFRESH_MS = 3000
const lastFetchedAt = ref(0)

watch(open, (isOpen) => {
  if (!isOpen) {
    confirmClearModel.value = false
    return
  }
  void refresh()
})

async function refresh() {
  try {
    await store.fetchEntries({ model: props.model })
  } finally {
    lastFetchedAt.value = Date.now()
  }
}

/**
 * Hover/focus preview: reuse a fresh store snapshot instead of refetching each
 * time. Focus is included because keyboard Tab also opens the tooltip.
 */
function ensureFresh() {
  if (store.loading) return
  if (Date.now() - lastFetchedAt.value < HOVER_REFRESH_MS) return
  void refresh()
}

/** Age / remaining-TTL labels reuse the shared duration formatter. */
function expiryLabel(entry: SessionAffinityEntry): string {
  if (entry.expires_in_secs === null) return t("sessionAffinity.neverExpires")
  if (entry.expires_in_secs <= 0) return t("common.expired")
  return t("sessionAffinity.expiresIn", { time: formatDuration(entry.expires_in_secs * 1000) })
}

/**
 * Expired leftovers stay visible but dimmed. The popover sits on the regular
 * popover surface, so it can use the muted colour; the tooltip inverts its
 * colours (`bg-foreground text-background`), where `text-muted-foreground`
 * would drop below readable contrast, so it only dims.
 */
function pinRowClass(entry: SessionAffinityEntry): string {
  return isPinActive(entry) ? "" : "text-muted-foreground opacity-50"
}

function tooltipRowClass(entry: SessionAffinityEntry): string {
  return isPinActive(entry) ? "" : "opacity-50"
}

async function clearEntry(entry: SessionAffinityEntry) {
  clearingKey.value = entry.key
  try {
    await store.deleteByKey(entry.key)
    toast.success(t("sessionAffinity.toast.entryCleared", { key: shortSessionKey(entry.key) }))
    await refresh()
    emit("changed")
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("sessionAffinity.toast.clearFailed")))
  } finally {
    clearingKey.value = null
  }
}

async function clearModel() {
  clearingModel.value = true
  try {
    const { deleted } = await store.deleteByModel(props.model)
    toast.success(t("sessionAffinity.toast.modelCleared", { model: props.model, count: deleted }))
    confirmClearModel.value = false
    await refresh()
    emit("changed")
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("sessionAffinity.toast.clearFailed")))
  } finally {
    clearingModel.value = false
  }
}
</script>

<template>
  <Popover v-model:open="open">
    <Tooltip :disabled="open">
      <TooltipTrigger as-child>
        <PopoverTrigger as-child>
          <Button
            variant="ghost"
            size="icon-sm"
            class="cursor-pointer"
            :aria-label="t('sessionAffinity.button')"
            @mouseenter="ensureFresh"
            @focus="ensureFresh"
          >
            <KeyRound class="h-3.5 w-3.5" />
          </Button>
        </PopoverTrigger>
      </TooltipTrigger>
      <!-- Hover preview: the same sorted pin list, read-only (clearing stays in
           the popover so a stray click can never drop a live session). -->
      <TooltipContent side="left" class="w-64 space-y-1 font-normal">
        <div class="flex items-center justify-between gap-2">
          <span class="font-medium">{{ t("sessionAffinity.title") }}</span>
          <span class="tabular-nums">
            {{ t("sessionAffinity.count", { count: rows.length }) }}
          </span>
        </div>
        <template v-if="rows.length > 0">
          <ul class="max-h-64 space-y-1 overflow-y-auto">
            <li v-for="entry in sortedRows" :key="entry.key" :class="tooltipRowClass(entry)">
              <div class="truncate font-mono">{{ entry.provider }}/{{ entry.real_model }}</div>
              <div class="flex items-center gap-1.5 text-[10px] tabular-nums">
                <span class="font-mono">{{ shortSessionKey(entry.key) }}</span>
                <span class="bg-background/15 rounded px-1">{{ entry.source }}</span>
                <span>{{ expiryLabel(entry) }}</span>
              </div>
            </li>
          </ul>
          <!-- A failed refresh must not hide the pins already known: show it as
               a non-blocking note under the list. -->
          <div v-if="refreshError" class="flex items-start gap-1 text-[10px] opacity-70">
            <TriangleAlert class="mt-px h-3 w-3 shrink-0" />
            <span class="break-words whitespace-normal">{{ refreshError }}</span>
          </div>
        </template>
        <div v-else-if="store.loading" class="flex items-center gap-1.5">
          <Loader2 class="h-3 w-3 animate-spin" />
          {{ t("sessionAffinity.loading") }}
        </div>
        <div v-else-if="refreshError" class="break-words whitespace-normal">
          {{ refreshError }}
        </div>
        <div v-else>{{ t("sessionAffinity.empty") }}</div>
      </TooltipContent>
    </Tooltip>
    <PopoverContent class="w-80 p-0" align="end">
      <!-- Header: model space + pin count -->
      <div class="border-border/60 flex items-start justify-between gap-2 border-b px-3 py-2">
        <div class="min-w-0">
          <div class="text-xs font-medium">{{ t("sessionAffinity.title") }}</div>
          <div class="text-muted-foreground truncate font-mono text-[10px]" :title="model">
            {{ model }}
          </div>
        </div>
        <span class="text-muted-foreground shrink-0 text-[10px] tabular-nums">
          {{ t("sessionAffinity.count", { count: rows.length }) }}
        </span>
      </div>

      <div class="max-h-64 overflow-y-auto">
        <template v-if="rows.length > 0">
          <ul class="divide-border/60 divide-y">
            <li
              v-for="entry in sortedRows"
              :key="entry.key"
              class="flex items-start gap-2 px-3 py-2"
              :class="pinRowClass(entry)"
            >
              <div class="min-w-0 flex-1 space-y-0.5">
                <div
                  class="truncate font-mono text-xs"
                  :title="`${entry.provider}/${entry.real_model}`"
                >
                  {{ entry.provider }}/{{ entry.real_model }}
                </div>
                <div class="text-muted-foreground flex items-center gap-1.5 text-[10px]">
                  <span class="font-mono" :title="entry.key">{{ shortSessionKey(entry.key) }}</span>
                  <span class="bg-muted rounded px-1">{{ entry.source }}</span>
                  <span>{{
                    t("sessionAffinity.age", { age: formatDuration(entry.age_secs * 1000) })
                  }}</span>
                </div>
                <div class="text-muted-foreground text-[10px]">{{ expiryLabel(entry) }}</div>
              </div>
              <Button
                variant="ghost"
                size="icon-sm"
                class="text-muted-foreground hover:text-destructive shrink-0"
                :disabled="clearingKey !== null"
                :title="t('sessionAffinity.clearEntry')"
                :aria-label="t('sessionAffinity.clearEntry')"
                @click="clearEntry(entry)"
              >
                <Loader2 v-if="clearingKey === entry.key" class="h-3 w-3 animate-spin" />
                <X v-else class="h-3.5 w-3.5" />
              </Button>
            </li>
          </ul>
          <!-- A failed refresh must not hide the pins already known (and must
               not take the clear buttons away): show it as a muted note. -->
          <div
            v-if="refreshError"
            class="text-muted-foreground flex items-start gap-1.5 px-3 py-2 text-[10px]"
          >
            <TriangleAlert class="mt-px h-3 w-3 shrink-0" />
            <span class="break-words whitespace-normal">{{ refreshError }}</span>
          </div>
        </template>
        <div
          v-else-if="store.loading"
          class="text-muted-foreground flex items-center gap-1.5 px-3 py-4 text-xs"
        >
          <Loader2 class="h-3 w-3 animate-spin" />
          {{ t("sessionAffinity.loading") }}
        </div>
        <div v-else-if="refreshError" class="text-destructive px-3 py-4 text-xs">
          {{ refreshError }}
        </div>
        <div v-else class="text-muted-foreground px-3 py-4 text-center text-xs">
          {{ t("sessionAffinity.empty") }}
        </div>
      </div>

      <!-- Footer: clear the whole model space, with a two-step confirm -->
      <div class="border-border/60 border-t p-2">
        <Button
          v-if="!confirmClearModel"
          variant="outline"
          size="sm"
          class="w-full"
          :disabled="rows.length === 0 || store.loading"
          @click="confirmClearModel = true"
        >
          {{ t("sessionAffinity.clearModel") }}
        </Button>
        <div v-else class="space-y-1.5">
          <p class="text-xs">
            {{ t("sessionAffinity.confirmClearModel", { count: rows.length }) }}
          </p>
          <div class="flex gap-1.5">
            <Button
              variant="outline"
              size="sm"
              class="flex-1"
              :disabled="clearingModel"
              @click="confirmClearModel = false"
            >
              {{ t("common.cancel") }}
            </Button>
            <Button
              variant="destructive"
              size="sm"
              class="flex-1"
              :disabled="clearingModel"
              @click="clearModel"
            >
              <Loader2 v-if="clearingModel" class="h-3 w-3 animate-spin" />
              {{ clearingModel ? t("sessionAffinity.clearing") : t("routeList.clear") }}
            </Button>
          </div>
        </div>
      </div>
    </PopoverContent>
  </Popover>
</template>

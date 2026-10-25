<script setup lang="ts">
import { KeyRound, Loader2, X } from "@lucide/vue"
import { computed, ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { t } from "@/lib/i18n"
import { shortSessionKey } from "@/lib/sessionAffinity"
import { formatDuration, getErrorMessage } from "@/lib/utils"
import { useSessionAffinityStore } from "@/stores/sessionAffinity"
import type { SessionAffinityEntry } from "@/types/api"

/**
 * Session-affinity pins of one `session_sticky` route: lists the live pins of
 * the route's request-model space and clears either a single session
 * (`DELETE { key }`) or the whole space (`DELETE { model }`).
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

watch(open, (isOpen) => {
  if (!isOpen) {
    confirmClearModel.value = false
    return
  }
  void refresh()
})

async function refresh() {
  await store.fetchEntries({ model: props.model })
}

/** Age / remaining-TTL labels reuse the shared duration formatter. */
function expiryLabel(entry: SessionAffinityEntry): string {
  if (entry.expires_in_secs === null) return t("sessionAffinity.neverExpires")
  return t("sessionAffinity.expiresIn", { time: formatDuration(entry.expires_in_secs * 1000) })
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
    <PopoverTrigger as-child>
      <Button
        variant="ghost"
        size="icon-sm"
        class="cursor-pointer"
        :title="t('sessionAffinity.button')"
        :aria-label="t('sessionAffinity.button')"
      >
        <KeyRound class="h-3.5 w-3.5" />
      </Button>
    </PopoverTrigger>
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
        <div
          v-if="store.loading"
          class="text-muted-foreground flex items-center gap-1.5 px-3 py-4 text-xs"
        >
          <Loader2 class="h-3 w-3 animate-spin" />
          {{ t("sessionAffinity.loading") }}
        </div>
        <div v-else-if="store.error" class="text-destructive px-3 py-4 text-xs">
          {{ store.error }}
        </div>
        <div
          v-else-if="rows.length === 0"
          class="text-muted-foreground px-3 py-4 text-center text-xs"
        >
          {{ t("sessionAffinity.empty") }}
        </div>
        <ul v-else class="divide-border/60 divide-y">
          <li v-for="entry in rows" :key="entry.key" class="flex items-start gap-2 px-3 py-2">
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

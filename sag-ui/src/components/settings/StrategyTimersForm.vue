<script setup lang="ts">
import { AlertCircle, Clock, Info, Loader2, Save } from "@lucide/vue"
import { ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { t } from "@/lib/i18n"
import { useGatewayConfigStore } from "@/stores/gatewayConfig"

const store = useGatewayConfigStore()

// Scheduling TTLs are whole seconds; 0 is a legal "no expiry" value.
const shuffleCacheTtlSecs = ref(7200)
const stickyFailoverResetSecs = ref(7200)
const formError = ref("")

/** Generous upper bound (one year) that only rejects absurd TTLs. */
const MAX_SECS = 31_536_000

function num(value: number | string): number {
  return typeof value === "number" ? value : Number(value)
}

function syncFromStore() {
  const config = store.config
  if (!config) return
  shuffleCacheTtlSecs.value = config.shuffle_cache_ttl_secs
  stickyFailoverResetSecs.value = config.sticky_failover_reset_secs
  formError.value = ""
}

watch(() => store.config, syncFromStore, { immediate: true })

function validateSecs(value: number | string, label: string): string | null {
  const secs = num(value)
  if (!Number.isInteger(secs) || secs < 0) {
    return t("settings.form.secsMin0", { label })
  }
  if (secs > MAX_SECS) {
    return t("settings.form.secsMax", { label, max: MAX_SECS })
  }
  return null
}

function validate(): string | null {
  return (
    validateSecs(shuffleCacheTtlSecs.value, t("settings.strategyTimers.fillFirstTtl")) ??
    validateSecs(stickyFailoverResetSecs.value, t("settings.strategyTimers.stickyReset"))
  )
}

async function handleSave() {
  formError.value = ""
  const validationError = validate()
  if (validationError) {
    formError.value = validationError
    return
  }
  try {
    // Both scalars are optional in the payload; omitted fields stay unchanged.
    await store.saveRuntimeConfig({
      shuffle_cache_ttl_secs: num(shuffleCacheTtlSecs.value),
      sticky_failover_reset_secs: num(stickyFailoverResetSecs.value),
    })
    toast.success(t("settings.toast.strategyTimersSaved"))
  } catch (e: any) {
    formError.value = store.error || String(e)
  }
}
</script>

<template>
  <Card>
    <CardHeader>
      <div class="max-w-3xl min-w-0 space-y-1.5">
        <CardTitle class="flex items-center gap-2">
          <Clock class="h-4 w-4" />
          {{ $t("settings.strategyTimers.formTitle") }}
          <Tooltip>
            <TooltipTrigger as-child>
              <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
            </TooltipTrigger>
            <TooltipContent side="top" class="max-w-xs font-normal">
              {{ $t("settings.strategyTimers.formDescription") }}
            </TooltipContent>
          </Tooltip>
        </CardTitle>
      </div>
    </CardHeader>
    <CardContent class="space-y-4">
      <div class="grid max-w-3xl gap-4 sm:grid-cols-2">
        <div class="space-y-2">
          <Label for="timers-shuffle" class="flex items-center gap-1.5">
            {{ $t("settings.strategyTimers.labelFillFirstTtl") }}
            <Tooltip>
              <TooltipTrigger as-child>
                <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
              </TooltipTrigger>
              <TooltipContent side="top" class="max-w-xs font-normal">
                {{ $t("settings.strategyTimers.hintFillFirstTtlLead") }}<code>0</code
                >{{ $t("settings.strategyTimers.hintFillFirstTtlTail") }}
              </TooltipContent>
            </Tooltip>
          </Label>
          <Input
            id="timers-shuffle"
            v-model.number="shuffleCacheTtlSecs"
            type="number"
            min="0"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
        </div>
        <div class="space-y-2">
          <Label for="timers-sticky" class="flex items-center gap-1.5">
            {{ $t("settings.strategyTimers.labelStickyReset") }}
            <Tooltip>
              <TooltipTrigger as-child>
                <Info class="text-muted-foreground h-3.5 w-3.5 cursor-help" />
              </TooltipTrigger>
              <TooltipContent side="top" class="max-w-xs font-normal">
                {{ $t("settings.strategyTimers.hintStickyReset") }}
              </TooltipContent>
            </Tooltip>
          </Label>
          <Input
            id="timers-sticky"
            v-model.number="stickyFailoverResetSecs"
            type="number"
            min="0"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
        </div>
      </div>

      <div
        v-if="formError"
        class="bg-destructive/10 text-destructive flex max-w-3xl items-start gap-2 rounded-md px-3 py-2 text-xs"
      >
        <AlertCircle class="mt-0.5 h-3.5 w-3.5 shrink-0" />
        <span>{{ formError }}</span>
      </div>

      <div class="flex flex-wrap items-center gap-2 border-t pt-4">
        <Button :disabled="store.saving" @click="handleSave">
          <Loader2 v-if="store.saving" class="h-3.5 w-3.5 animate-spin" />
          <Save v-else class="h-3.5 w-3.5" />
          {{ $t("common.save") }}
        </Button>
      </div>
    </CardContent>
  </Card>
</template>

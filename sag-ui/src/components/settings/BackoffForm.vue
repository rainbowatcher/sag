<script setup lang="ts">
import { AlertCircle, Loader2, Save, Timer } from "@lucide/vue"
import { ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { t } from "@/lib/i18n"
import { useGatewayConfigStore } from "@/stores/gatewayConfig"

const store = useGatewayConfigStore()

const retries = ref(2)
const baseMs = ref(100)
const maxMs = ref(5000)
const jitter = ref(true)
const retryOnEmpty = ref(true)
const formError = ref("")

function num(value: number | string): number {
  return typeof value === "number" ? value : Number(value)
}

function syncFromStore() {
  const backoff = store.config?.backoff
  if (!backoff) return
  retries.value = backoff.retries
  baseMs.value = backoff.base_ms
  maxMs.value = backoff.max_ms
  jitter.value = backoff.jitter
  retryOnEmpty.value = backoff.retry_on_empty_response
  formError.value = ""
}

watch(() => store.config, syncFromStore, { immediate: true })

function validate(): string | null {
  const retriesNum = num(retries.value)
  const baseNum = num(baseMs.value)
  const maxNum = num(maxMs.value)
  if (!Number.isFinite(retriesNum) || retriesNum < 1) return t("settings.backoff.retriesMin")
  if (!Number.isFinite(baseNum) || baseNum < 0) return t("settings.backoff.baseNonNegative")
  if (!Number.isFinite(maxNum) || maxNum < 0) return t("settings.backoff.maxNonNegative")
  if (baseNum > maxNum) return t("settings.backoff.baseExceedsMax")
  return null
}

async function handleSave() {
  formError.value = ""
  const validationError = validate()
  if (validationError) {
    formError.value = validationError
    return
  }
  try {
    await store.saveRuntimeConfig({
      backoff: {
        retries: num(retries.value),
        base_ms: num(baseMs.value),
        max_ms: num(maxMs.value),
        jitter: jitter.value,
        retry_on_empty_response: retryOnEmpty.value,
      },
    })
    toast.success(t("settings.toast.backoffSaved"))
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
          <Timer class="h-4 w-4" />
          {{ $t("settings.backoff.formTitle") }}
        </CardTitle>
        <CardDescription>
          {{ $t("settings.backoff.formDescription") }}
        </CardDescription>
      </div>
    </CardHeader>
    <CardContent class="space-y-4">
      <div class="grid max-w-3xl gap-4 sm:grid-cols-3">
        <div class="space-y-2">
          <Label for="backoff-retries">{{ $t("settings.backoff.labelRetries") }}</Label>
          <Input
            id="backoff-retries"
            v-model.number="retries"
            type="number"
            min="1"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">{{ $t("settings.backoff.hintRetries") }}</p>
        </div>
        <div class="space-y-2">
          <Label for="backoff-base">{{ $t("settings.backoff.labelBase") }}</Label>
          <Input
            id="backoff-base"
            v-model.number="baseMs"
            type="number"
            min="0"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">{{ $t("settings.backoff.hintBase") }}</p>
        </div>
        <div class="space-y-2">
          <Label for="backoff-max">{{ $t("settings.backoff.labelMax") }}</Label>
          <Input
            id="backoff-max"
            v-model.number="maxMs"
            type="number"
            min="0"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">{{ $t("settings.backoff.hintMax") }}</p>
        </div>
      </div>

      <div class="max-w-3xl space-y-3 border-t pt-4">
        <div class="flex items-center justify-between gap-4">
          <div class="space-y-0.5">
            <Label for="backoff-jitter">{{ $t("settings.backoff.labelJitter") }}</Label>
            <p class="text-muted-foreground text-xs">
              {{ $t("settings.backoff.hintJitter") }}
            </p>
          </div>
          <Switch id="backoff-jitter" v-model="jitter" :disabled="store.saving" />
        </div>
        <div class="flex items-center justify-between gap-4">
          <div class="space-y-0.5">
            <Label for="backoff-empty">{{ $t("settings.backoff.labelRetryOnEmpty") }}</Label>
            <p class="text-muted-foreground text-xs">
              {{ $t("settings.backoff.hintRetryOnEmpty") }}
            </p>
          </div>
          <Switch id="backoff-empty" v-model="retryOnEmpty" :disabled="store.saving" />
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

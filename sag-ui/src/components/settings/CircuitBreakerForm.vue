<script setup lang="ts">
import { AlertCircle, Loader2, Save, Zap } from "@lucide/vue"
import { ref, watch } from "vue"
import { toast } from "vue-sonner"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useGatewayConfigStore } from "@/stores/gatewayConfig"

const store = useGatewayConfigStore()

// Cooldowns and the probe cadence are whole seconds and must be > 0 (the
// backend rejects 0).
const quotaExhaustedSecs = ref(3600)
const rateLimitedSecs = ref(10)
const genericSecs = ref(60)
const probeIntervalSecs = ref(60)
const formError = ref("")

/** Generous upper bound (one year) that only rejects absurd cooldowns. */
const MAX_SECS = 31_536_000

function num(value: number | string): number {
  return typeof value === "number" ? value : Number(value)
}

function syncFromStore() {
  const breaker = store.config?.circuit_breaker
  if (!breaker) return
  quotaExhaustedSecs.value = breaker.quota_exhausted_secs
  rateLimitedSecs.value = breaker.rate_limited_secs
  genericSecs.value = breaker.generic_secs
  probeIntervalSecs.value = breaker.probe_interval_secs
  formError.value = ""
}

watch(() => store.config, syncFromStore, { immediate: true })

function validateSecs(value: number | string, label: string): string | null {
  const secs = num(value)
  if (!Number.isInteger(secs) || secs < 1) {
    return `${label} must be a whole number of seconds (≥ 1).`
  }
  if (secs > MAX_SECS) {
    return `${label} must not exceed ${MAX_SECS} seconds (one year).`
  }
  return null
}

function validate(): string | null {
  return (
    validateSecs(quotaExhaustedSecs.value, "Quota-exhausted cooldown") ??
    validateSecs(rateLimitedSecs.value, "Rate-limited cooldown") ??
    validateSecs(genericSecs.value, "Generic cooldown") ??
    validateSecs(probeIntervalSecs.value, "Probe interval")
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
    // Full-object replace: every field of the object is required by the API,
    // so all four are always sent together.
    await store.saveRuntimeConfig({
      circuit_breaker: {
        quota_exhausted_secs: num(quotaExhaustedSecs.value),
        rate_limited_secs: num(rateLimitedSecs.value),
        generic_secs: num(genericSecs.value),
        probe_interval_secs: num(probeIntervalSecs.value),
      },
    })
    toast.success("Circuit breaker settings saved")
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
          <Zap class="h-4 w-4" />
          Circuit Breaker Cooldowns
        </CardTitle>
        <CardDescription>
          How long a failing target stays tripped and how often it is probed while cooling down.
          Changes take effect for new requests immediately.
        </CardDescription>
      </div>
    </CardHeader>
    <CardContent class="space-y-4">
      <div class="grid max-w-3xl gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <div class="space-y-2">
          <Label for="breaker-quota">Quota exhausted (s)</Label>
          <Input
            id="breaker-quota"
            v-model.number="quotaExhaustedSecs"
            type="number"
            min="1"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">
            Cooldown after an upstream quota-exhausted (403) failure.
          </p>
        </div>
        <div class="space-y-2">
          <Label for="breaker-rate">Rate limited (s)</Label>
          <Input
            id="breaker-rate"
            v-model.number="rateLimitedSecs"
            type="number"
            min="1"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">Cooldown after a 429 rate-limit response.</p>
        </div>
        <div class="space-y-2">
          <Label for="breaker-generic">Generic (s)</Label>
          <Input
            id="breaker-generic"
            v-model.number="genericSecs"
            type="number"
            min="1"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">Cooldown for any other retryable failure.</p>
        </div>
        <div class="space-y-2">
          <Label for="breaker-probe">Probe interval (s)</Label>
          <Input
            id="breaker-probe"
            v-model.number="probeIntervalSecs"
            type="number"
            min="1"
            step="1"
            class="font-mono"
            :disabled="store.saving"
          />
          <p class="text-muted-foreground text-xs">
            How often an open circuit is probed while it cools down.
          </p>
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
          Save
        </Button>
      </div>
    </CardContent>
  </Card>
</template>

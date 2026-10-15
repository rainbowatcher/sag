<script setup lang="ts">
import {
  AlertTriangle,
  RefreshCw,
  Loader2,
  ShieldCheck,
  Zap,
  Clock,
  Activity,
  RotateCcw,
} from "@lucide/vue"
import { computed, ref } from "vue"
import { toast } from "vue-sonner"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { t } from "@/lib/i18n"
import { getErrorMessage, formatRemaining } from "@/lib/utils"
import { useCircuitBreakerStore } from "@/stores/circuitBreaker"

const store = useCircuitBreakerStore()

const sortedBreakers = computed(() =>
  [...store.breakers].sort((a, b) => {
    // Open first, then HalfOpen; within each group, sort by model then provider
    if (a.state !== b.state) return a.state === "open" ? -1 : 1
    return a.model.localeCompare(b.model) || a.provider.localeCompare(b.provider)
  }),
)

async function refresh() {
  await store.fetchBreakers()
}

// Track which target is currently being force-closed (for button loading state).
const closingKey = ref<string | null>(null)

async function forceClose(model: string, provider: string, realModel: string) {
  const key = `${model}-${provider}-${realModel}`
  closingKey.value = key
  try {
    await store.closeBreaker(model, provider, realModel)
    toast.success(t("circuitBreaker.recovered", { provider, model: realModel }))
  } catch (e: any) {
    toast.error(getErrorMessage(e, t("circuitBreaker.restoreFailed")))
  } finally {
    closingKey.value = null
  }
}
</script>

<template>
  <Card>
    <CardHeader class="flex flex-row items-center justify-between space-y-0 pb-3">
      <div class="flex items-center gap-2">
        <AlertTriangle
          class="h-4 w-4"
          :class="store.breakers.length > 0 ? 'text-amber-500' : 'text-muted-foreground'"
        />
        <CardTitle class="text-sm font-medium">{{ $t("circuitBreaker.title") }}</CardTitle>
        <Badge v-if="store.breakers.length > 0" variant="destructive" class="text-[10px]">
          {{ $t("circuitBreaker.tripped", { count: store.breakers.length }) }}
        </Badge>
      </div>
      <Button
        variant="ghost"
        size="icon-sm"
        :disabled="store.loading"
        @click="refresh()"
        :title="$t('circuitBreaker.refreshTitle')"
      >
        <RefreshCw :class="store.loading ? 'animate-spin' : ''" class="h-3.5 w-3.5" />
      </Button>
    </CardHeader>
    <CardContent>
      <!-- Loading -->
      <div
        v-if="store.loading && store.breakers.length === 0"
        class="text-muted-foreground flex items-center justify-center py-8"
      >
        <Loader2 class="h-5 w-5 animate-spin" />
      </div>

      <!-- Error -->
      <div v-else-if="store.error" class="text-destructive py-4 text-center text-sm">
        {{ store.error }}
      </div>

      <!-- Empty: all healthy -->
      <div
        v-else-if="store.breakers.length === 0"
        class="text-muted-foreground flex items-center justify-center gap-2 py-8 text-sm"
      >
        <ShieldCheck class="h-4 w-4 text-green-500" />
        {{ $t("circuitBreaker.allHealthy") }}
      </div>

      <!-- Breaker list -->
      <div v-else class="space-y-2">
        <div
          v-for="b in sortedBreakers"
          :key="`${b.model}-${b.provider}-${b.real_model}`"
          class="hover:bg-accent/50 flex items-center gap-3 rounded-md border p-3 transition-colors"
        >
          <!-- State badge -->
          <div class="flex flex-col items-center gap-1">
            <Badge
              :variant="b.state === 'open' ? 'destructive' : 'secondary'"
              class="text-[10px] uppercase"
              :class="
                b.state === 'half_open' ? 'border-amber-500/30 bg-amber-500/10 text-amber-600' : ''
              "
            >
              {{
                b.state === "open"
                  ? $t("circuitBreaker.stateOpen")
                  : $t("circuitBreaker.stateHalfOpen")
              }}
            </Badge>
            <div
              v-if="b.probe_in_flight"
              class="flex items-center gap-0.5 text-[10px] text-amber-600"
              :title="$t('circuitBreaker.probingTitle')"
            >
              <Zap class="h-2.5 w-2.5" />
              {{ $t("circuitBreaker.probing") }}
            </div>
          </div>

          <!-- Target identity -->
          <div class="min-w-0 flex-1">
            <div class="flex items-baseline gap-1.5">
              <span class="truncate text-sm font-medium">{{ b.model }}</span>
              <span class="text-muted-foreground text-xs">→</span>
              <span class="text-muted-foreground truncate font-mono text-xs">
                {{ b.provider }}/{{ b.real_model }}
              </span>
            </div>

            <!-- Time info -->
            <div class="mt-1 flex flex-wrap items-center gap-x-4 gap-y-0.5 text-xs">
              <span class="text-muted-foreground flex items-center gap-1">
                <Clock class="h-3 w-3" />
                {{ $t("circuitBreaker.cooldown") }}
                <span
                  :class="
                    b.cooldown_until_secs * 1000 <= Date.now()
                      ? 'font-medium text-green-600'
                      : 'text-foreground font-medium'
                  "
                >
                  {{ formatRemaining(b.cooldown_until_secs, $t("circuitBreaker.cooldownElapsed")) }}
                </span>
              </span>
              <span
                v-if="b.next_probe_at_secs !== null"
                class="text-muted-foreground flex items-center gap-1"
              >
                <Activity class="h-3 w-3" />
                {{ $t("circuitBreaker.nextProbe") }}
                <span
                  :class="
                    b.next_probe_at_secs * 1000 <= Date.now()
                      ? 'font-medium text-amber-600'
                      : 'text-foreground font-medium'
                  "
                >
                  {{ formatRemaining(b.next_probe_at_secs, $t("circuitBreaker.probeElapsed")) }}
                </span>
              </span>
              <span v-else class="flex items-center gap-1 font-medium text-amber-600">
                <Activity class="h-3 w-3" />
                {{ $t("circuitBreaker.probeReady") }}
              </span>
            </div>
          </div>

          <!-- Force close button -->
          <Button
            variant="outline"
            size="sm"
            class="shrink-0 text-xs"
            :disabled="closingKey === `${b.model}-${b.provider}-${b.real_model}`"
            @click="forceClose(b.model, b.provider, b.real_model)"
            :title="$t('circuitBreaker.forceCloseTitle')"
          >
            <Loader2
              v-if="closingKey === `${b.model}-${b.provider}-${b.real_model}`"
              class="h-3 w-3 animate-spin"
            />
            <RotateCcw v-else class="h-3 w-3" />
            {{ $t("circuitBreaker.restore") }}
          </Button>
        </div>
      </div>
    </CardContent>
  </Card>
</template>

import { defineStore } from "pinia"
import { ref, computed } from "vue"
import { api } from "@/lib/client"
import { getErrorMessage } from "@/lib/utils"
import type { CircuitBreakerEntry } from "@/types/api"
import { useConnectionStore } from "./connection"

export const useCircuitBreakerStore = defineStore("circuitBreaker", () => {
  const breakers = ref<CircuitBreakerEntry[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetchBreakers() {
    loading.value = true
    error.value = null
    try {
      const { data } = await api.get<{ breakers: CircuitBreakerEntry[] }>("/api/circuit-breaker")
      breakers.value = data.breakers
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      error.value = getErrorMessage(e, "Failed to load circuit breaker state")
    } finally {
      loading.value = false
    }
  }

  async function closeBreaker(model: string, provider: string, realModel: string) {
    try {
      await api.post("/api/circuit-breaker/close", {
        model,
        provider,
        real_model: realModel,
      })
    } catch (e: any) {
      if (e?.response?.status === 401) useConnectionStore().handle401()
      throw e
    }
    // Refresh after closing so the UI reflects the updated state.
    await fetchBreakers()
  }

  const openBreakers = computed(() => breakers.value.filter((b) => b.state === "open"))
  const halfOpenBreakers = computed(() => breakers.value.filter((b) => b.state === "half_open"))

  return {
    breakers,
    loading,
    error,
    fetchBreakers,
    closeBreaker,
    openBreakers,
    halfOpenBreakers,
  }
})

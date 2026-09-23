import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"

export const useHealthStore = defineStore("health", () => {
  const gatewayVersion = ref("")
  const gatewayOnline = ref(false)

  async function checkHealth() {
    try {
      const { data } = await api.get<{ status: string; version?: string }>("/health")
      gatewayOnline.value = data.status === "ok"
      gatewayVersion.value = data.version ?? gatewayVersion.value
    } catch {
      gatewayOnline.value = false
    }
  }

  return {
    gatewayVersion,
    gatewayOnline,
    checkHealth,
  }
})

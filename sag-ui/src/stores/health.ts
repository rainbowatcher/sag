import { defineStore } from "pinia"
import { ref } from "vue"
import { api } from "@/lib/client"

export const useHealthStore = defineStore("health", () => {
  const gatewayOnline = ref(false)

  async function checkHealth() {
    try {
      const { data } = await api.get<{ status: string }>("/health")
      gatewayOnline.value = data.status === "ok"
    } catch {
      gatewayOnline.value = false
    }
  }

  return {
    gatewayOnline,
    checkHealth,
  }
})

import axios from "axios"

export const api = axios.create({ baseURL: "" })

export function headers(apiKey?: string) {
  return apiKey ? { Authorization: `Bearer ${apiKey}` } : {}
}

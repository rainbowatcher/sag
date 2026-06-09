export interface Provider {
  name: string
  provider_type: string
  base_url: string
  priority: number
  max_retries: number
  timeout_secs: number
  supported_params: string[]
  headers: Record<string, string>
  body_overrides: Record<string, any>
}

export interface RouteTarget {
  provider: string
  real_model: string | null
  priority: number
}

export interface RouteConfig {
  model_name: string
  api_endpoint: string
  strategy: Strategy
  targets: RouteTarget[]
}

export type Strategy = "fill_first" | "round_robin"

export interface CreateRoutePayload {
  model_name: string
  api_endpoint?: string
  strategy?: Strategy
  targets: Array<{ provider: string; real_model?: string; priority?: number }>
}

export interface UpdateRoutePayload {
  model_name: string
  api_endpoint?: string
  strategy?: Strategy
  targets?: Array<{ provider: string; real_model?: string; priority?: number }>
}

export interface StatsRow {
  provider: string
  model: string
  call_count: number
  total_tokens: number
  avg_duration_ms: number
  error_count: number
}

export interface Provider {
  name: string
  provider_type: string
  base_url: string
  priority: number
  max_retries: number
  timeout_secs: number
  supported_params: string[]
}

export interface RouteTarget {
  provider: string
  real_model: string
  priority: number
}

export interface RouteConfig {
  model_name: string
  api_endpoint: string
  targets: RouteTarget[]
}

export type Strategy = "fill_first" | "round_robin"

export interface AddRoutePayload {
  model_name: string
  real_model?: string
  api_endpoint?: string
  priority?: number
  strategy?: Strategy
}

export interface StatsRow {
  provider: string
  model: string
  call_count: number
  total_tokens: number
  avg_duration_ms: number
  error_count: number
}

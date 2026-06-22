export interface Provider {
  name: string
  api_keys: string[]
  key_strategy: "fill_first" | "round_robin"
  provider_type: string
  base_url: string
  models_endpoint: string
  timeout_secs: number
  headers: Record<string, string>
  body_overrides: Record<string, any>
  enabled_models: string[]
  enabled: boolean
}

export interface ModelTestResult {
  key_prefix: string
  success: boolean
  message: string
}

export interface RouteTarget {
  provider: string
  real_model: string | null
  priority: number
  retries?: number
  extra?: Record<string, any>
}

export interface TargetItem {
  provider: string
  real_model: string
  priority: number
  retries: number
  extra: string
}

export interface RouteConfig {
  model_name: string
  api_endpoint: string
  strategy: Strategy
  retries?: number
  targets: RouteTarget[]
}

export type Strategy = "fill_first" | "round_robin"

export interface CreateRoutePayload {
  model_name: string
  api_endpoint?: string
  strategy?: Strategy
  retries?: number
  targets: Array<{
    provider: string
    real_model?: string
    priority?: number
    retries?: number
    extra?: Record<string, any>
  }>
}

export interface UpdateRoutePayload {
  model_name: string
  api_endpoint?: string
  strategy?: Strategy
  retries?: number
  targets?: Array<{
    provider: string
    real_model?: string
    priority?: number
    retries?: number
    extra?: Record<string, any>
  }>
}

export interface StatsRow {
  provider: string
  model: string
  real_model: string
  call_count: number
  total_prompt_tokens: number
  total_completion_tokens: number
  total_tokens: number
  avg_duration_ms: number
  error_count: number
}

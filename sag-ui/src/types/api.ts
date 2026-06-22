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

/** Token usage block returned by upstream providers (mirrors sag_core::Usage). */
export interface Usage {
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
  cached_tokens?: number
  cache_creation_tokens?: number
  reasoning_tokens?: number
}

/** Single row of the call log returned by `GET /api/logs` (mirrors sag_core::CallLog). */
export interface CallLog {
  request_id: string
  provider: string
  model: string
  real_model: string
  stream: boolean
  status_code: number
  duration_ms: number
  usage?: Usage
  error?: string
  /** RFC3339 timestamp string from chrono::DateTime<Utc>. */
  timestamp: string
  /** Set when the client sent `reasoning_effort` (OpenAI o-series). */
  reasoning_effort?: string
  /**
   * Stage-2 timing field: wall-clock duration from immediately before
   * the upstream provider call to the final response (non-streaming)
   * or stream end (streaming). Excludes SAG-internal overhead. Populated
   * for both streaming and non-streaming successes.
   */
  upstream_duration_ms?: number
  /**
   * Stage-2 timing field: time-to-first-token in milliseconds
   * (streaming only — non-streaming responses have `ttft_ms ==
   * upstream_duration_ms` since the whole response is the first byte).
   * `None` for streams that emit only metadata (Usage/Error) before
   * terminating, since no user-visible first byte ever arrived.
   */
  ttft_ms?: number
  cached_tokens?: number
  cache_creation_tokens?: number
  reasoning_tokens?: number
}

/** Query parameters accepted by `GET /api/logs`. */
export interface LogFilter {
  provider?: string
  model?: string
  real_model?: string
  status_code?: number
  offset?: number
  limit?: number
}

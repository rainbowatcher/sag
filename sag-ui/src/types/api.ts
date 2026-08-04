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
  strip_fields: string[]
  model_overrides: Record<string, Record<string, any>>
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
  id: number
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

export type Strategy = "sticky_failover" | "fill_first" | "round_robin"

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

/** Cache hit-rate statistics returned by `GET /v1/analytics/cache`. */
export interface CacheStatsRow {
  provider: string
  model: string
  real_model: string
  total_calls: number
  total_prompt_tokens: number
  total_cached_tokens: number
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
  /** Wall-clock upstream duration from provider call start to final response / stream end. */
  upstream_duration_ms?: number
  /** Time-to-first-token in ms; `undefined` if no user-visible byte arrived. */
  ttft_ms?: number
  cached_tokens?: number
  cache_creation_tokens?: number
  reasoning_tokens?: number
  /** User-Agent header from the client HTTP request. */
  user_agent?: string
  /** Client IP from X-Forwarded-For / X-Real-IP headers. */
  client_ip?: string
  /** Machine-readable error category (e.g. "model_not_found", "provider_error", "timeout"). */
  error_category?: string
}

/** Time-bucketed trend row returned by `GET /v1/analytics/trend`. */
export interface TrendRow {
  bucket: number
  call_count: number
  prompt_tokens: number
  completion_tokens: number
  success_count: number
}

/** Distribution row returned by `GET /v1/analytics/distribution`. */
export interface DistributionRow {
  provider: string
  model: string
  real_model: string
  call_count: number
  percentage: number
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

/** Response envelope returned by `GET /api/logs`. */
export interface LogsResponse {
  data: CallLog[]
  total: number
}

/** Circuit breaker entry returned by `GET /api/circuit-breaker`. */
export interface CircuitBreakerEntry {
  model: string
  provider: string
  real_model: string
  state: "open" | "half_open"
  /** UNIX timestamp (seconds) of the hard cooldown lower bound. */
  cooldown_until_secs: number
  /** `null` when HalfOpen (probe ready, no future schedule).
   *  `number` when Open — next moment a probe is allowed (UNIX seconds). */
  next_probe_at_secs: number | null
  /** Whether a probe request is currently in flight (HalfOpen only). */
  probe_in_flight: boolean
}

/** Sticky-failover active target entry returned by `GET /api/sticky-targets`. */
export interface StickyTargetEntry {
  model: string
  provider: string
  real_model: string
  age_secs: number
  expired: boolean
}

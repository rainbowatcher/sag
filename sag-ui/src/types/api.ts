/**
 * How a provider resolves the upstream HTTP proxy.
 * - `inherit` — follow the gateway-level `proxy_url` (default).
 * - `enabled` — force using the gateway-level `proxy_url`.
 * - `disabled` — force a direct connection, ignoring any global proxy.
 */
export type ProxyMode = "inherit" | "enabled" | "disabled"

/**
 * Key of a provider `apis` section: the three chat-family protocols plus
 * the non-protocol `image` entry (its path comes from route.`api_endpoint`).
 * Unknown keys are rejected by the backend (strict enum).
 */
export type ApiKind = "chat_completions" | "responses" | "anthropic_messages" | "image"

/** The three protocol keys of `ApiKind` (excludes `image`). */
export type ProtocolApiKind = Exclude<ApiKind, "image">

/**
 * One `providers.<name>.apis.<key>` section. `base_url` is required (the
 * only address source — there is no provider-level `base_url`); every other
 * field is optional and, when set, **fully replaces** the provider-level
 * default of the same name (maps are not merged, arrays not unioned).
 */
export interface ApiConfig {
  /** Upstream API root (host + root prefix, no protocol path). */
  base_url: string
  /** Relative path for the models listing call (default `"models"`). */
  models_endpoint?: string
  headers?: Record<string, string>
  body_overrides?: Record<string, any>
  strip_fields?: string[]
  model_strip_fields?: Record<string, string[]>
  model_overrides?: Record<string, Record<string, any>>
  role_mappings?: Record<string, string>
  anthropic_cache_control?: boolean
  timeout_secs?: number
  chunk_timeout_secs?: number
  ttfb_timeout_secs?: number
}

/** Fixed-key capability override on a route target (all keys optional). */
export interface CapabilitiesOverride {
  streaming?: boolean
  reasoning?: boolean
  tools?: boolean
  images?: boolean
  documents?: boolean
  stateful?: boolean
  hosted_tools?: boolean
  background?: boolean
}

export interface Provider {
  name: string
  api_keys: string[]
  key_strategy: "fill_first" | "round_robin"
  /** Protocol API catalog: one section per declared ApiKind. */
  apis: Record<ApiKind, ApiConfig>
  /**
   * Downgrade protocol for translated-lane dispatch. Implicit (auto) when
   * exactly one protocol key is declared; required when several are.
   */
  fallback?: ApiKind | null
  timeout_secs: number
  chunk_timeout_secs?: number
  ttfb_timeout_secs?: number
  headers: Record<string, string>
  /** Role mapping table: maps request roles to upstream-supported roles (e.g. developer -> system). */
  role_mappings: Record<string, string>
  body_overrides: Record<string, any>
  strip_fields: string[]
  model_strip_fields: Record<string, string[]>
  model_overrides: Record<string, Record<string, any>>
  enabled_models: string[]
  enabled: boolean
  /** How this provider resolves the upstream HTTP proxy. */
  proxy_mode: ProxyMode
  /** Per-ApiKind capability bitmap (read-only): `{ "<api_kind>": { ...capabilities } }`. */
  capabilities?: Record<string, Record<string, boolean>>
}

/** Payload of `POST /api/providers`. */
export interface AddProviderPayload {
  name: string
  api_keys: string[]
  key_strategy?: "fill_first" | "round_robin"
  apis: Record<ApiKind, ApiConfig>
  /** Required when `apis` declares more than one protocol key. */
  fallback?: ApiKind
  timeout_secs?: number
  headers?: Record<string, string>
  body_overrides?: Record<string, any>
  strip_fields?: string[]
  model_strip_fields?: Record<string, string[]>
  model_overrides?: Record<string, Record<string, any>>
  role_mappings?: Record<string, string>
  enabled_models?: string[]
  proxy_mode?: ProxyMode
}

/** Payload of `PUT /api/providers`. */
export interface UpdateProviderPayload {
  name: string
  api_keys?: string[]
  key_strategy?: "fill_first" | "round_robin"
  /** Whole-directory replace: sections not repeated are dropped. */
  apis?: Record<ApiKind, ApiConfig>
  /** Triple-state: omit → keep, `null` → clear, value → set. */
  fallback?: ApiKind | null
  timeout_secs?: number
  headers?: Record<string, string>
  body_overrides?: Record<string, any>
  strip_fields?: string[]
  model_strip_fields?: Record<string, string[]>
  model_overrides?: Record<string, Record<string, any>>
  role_mappings?: Record<string, string>
  enabled_models?: string[]
  enabled?: boolean
  proxy_mode?: ProxyMode
}

/**
 * Exponential-backoff tuning returned by `GET /api/config` (`[backoff]`).
 * A full object replace is applied on `PUT /api/config`.
 */
export interface BackoffConfig {
  retries: number
  base_ms: number
  max_ms: number
  jitter: boolean
  retry_on_empty_response: boolean
}

/**
 * Circuit-breaker cooldowns plus the half-open probe cadence returned by
 * `GET /api/config` (`[circuit_breaker]`). A full object replace is applied
 * on `PUT /api/config`, so all four fields are sent together.
 */
export interface CircuitBreakerConfigForm {
  quota_exhausted_secs: number
  rate_limited_secs: number
  generic_secs: number
  probe_interval_secs: number
}

/** Analytics storage config; `null` when the section is absent. */
export interface AnalyticsConfig {
  enabled: boolean
  storage_dsn: string
}

/** Gateway listen address (`[server]`); restart-required. */
export interface ServerConfig {
  host: string
  port: number
}

/** Logging output config (`[logging]`); restart-required. */
export interface LoggingConfig {
  level: string
  format: string
}

/**
 * Full gateway config snapshot returned by `GET /api/config`. Only the
 * runtime-tuning fields are writable via `PUT /api/config`; `server`,
 * `logging` and `analytics` are read-only and apply at process startup.
 */
export interface GatewayConfigSnapshot {
  server: ServerConfig
  logging: LoggingConfig
  analytics: AnalyticsConfig | null
  backoff: BackoffConfig
  circuit_breaker: CircuitBreakerConfigForm
  shuffle_cache_ttl_secs: number
  sticky_failover_reset_secs: number
  proxy_url: string | null
}

/**
 * Payload of `PUT /api/config`. Every field is optional; omitted fields
 * stay unchanged. `backoff` / `circuit_breaker` are full-object replaces.
 * `server` / `logging` / `analytics` are not accepted (restart-required).
 */
export interface UpdateGatewayConfigPayload {
  backoff?: BackoffConfig
  circuit_breaker?: CircuitBreakerConfigForm
  shuffle_cache_ttl_secs?: number
  sticky_failover_reset_secs?: number
}

/** Gateway-level settings returned by `GET /api/settings`. */
export interface Settings {
  /** Global upstream HTTP proxy URL; `null` when none is configured. */
  proxy_url: string | null
  /**
   * Gateway client-auth API keys (`[gateway_auth].api_keys`). Empty array
   * means authentication is disabled.
   */
  api_keys: string[]
}

/** Payload of `PUT /api/settings`. */
export interface UpdateSettingsPayload {
  /** Empty string or `null` clears the global proxy. */
  proxy_url: string | null
  /**
   * Omit to leave the current key list unchanged. An empty array disables
   * authentication.
   */
  api_keys?: string[]
}

export interface ModelTestResult {
  key_prefix: string
  success: boolean
  message: string
}

/**
 * Verdict of one protocol probe attempt (`POST /api/providers/probe-protocols`).
 * - `supported` — 2xx, endpoint accepted the minimal request.
 * - `auth_failed` — 401/403: the path exists but auth was rejected (not "unsupported").
 * - `uncertain` — 404 (model_not_found vs unknown route, see message) or other odd statuses.
 * - `unsupported` — 405: the path does not serve this protocol.
 * - `partial` — other 4xx: the path exists, the schema/params were rejected.
 * - `error` — transport failure (connect/timeout), no HTTP verdict.
 */
export type ProbeVerdict =
  | "supported"
  | "auth_failed"
  | "uncertain"
  | "unsupported"
  | "partial"
  | "error"

/** One probe target: a protocol plus the base_url to test it against. */
export interface ProbeTarget {
  protocol: ProtocolApiKind
  base_url: string
  /** Extra headers for this target (simulate `apis.<protocol>.headers`). */
  headers?: Record<string, string>
}

/** Payload of `POST /api/providers/probe-protocols` (read-only, never persisted). */
export interface ProbeProtocolsPayload {
  api_keys: string[]
  model: string
  /** Fixed set of the three probeable protocols (image is rejected with 400). */
  targets: ProbeTarget[]
  /** Extra headers applied to every target. */
  headers?: Record<string, string>
  proxy_mode?: ProxyMode
}

/** One row of the probe support matrix returned by the probe endpoint. */
export interface ProbeResult {
  protocol: ProtocolApiKind
  url: string
  key_prefix: string
  /** HTTP status; `null` on transport failure. */
  status: number | null
  message: string
  verdict: ProbeVerdict
}

export interface RouteTarget {
  provider: string
  real_model: string | null
  priority: number
  retries?: number
  extra?: Record<string, any>
  capabilities?: CapabilitiesOverride | null
}

export interface TargetItem {
  id: number
  provider: string
  real_model: string
  priority: number
  retries: number
  extra: string
}

/** Dispatch pipeline of a route (default `chat`). */
export type Pipeline = "chat" | "image"

export interface RouteConfig {
  model_name: string
  pipeline: Pipeline
  /** Real upstream path — only legal when `pipeline = "image"`; `null` otherwise. */
  api_endpoint: string | null
  strategy: Strategy
  retries?: number
  targets: RouteTarget[]
}

export type Strategy = "sticky_failover" | "fill_first" | "round_robin"

export interface CreateRoutePayload {
  model_name: string
  pipeline?: Pipeline
  /** Required when `pipeline = "image"`; must be omitted for chat routes. */
  api_endpoint?: string
  strategy?: Strategy
  retries?: number
  targets: Array<{
    provider: string
    real_model?: string
    priority?: number
    retries?: number
    extra?: Record<string, any>
    capabilities?: CapabilitiesOverride
  }>
}

export interface UpdateRoutePayload {
  model_name: string
  pipeline?: Pipeline
  /** Triple-state: omit → keep, `null` → clear, value → set. */
  api_endpoint?: string | null
  strategy?: Strategy
  retries?: number
  targets?: Array<{
    provider: string
    real_model?: string
    priority?: number
    retries?: number
    extra?: Record<string, any>
    capabilities?: CapabilitiesOverride
  }>
}

/** Optional model list metadata entry extras (`GET /v1/models`). */
export interface ModelListEntry {
  id: string
  object: "model"
  created: number
  owned_by: string
  /** Union of the `apis` protocol keys of every target's provider. Key name per plan §7.5. */
  ingress_protocols?: string[]
  lanes?: string[]
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
  /** Ingress protocol of the request (e.g. "chat_completions"). */
  ingress_protocol?: string
  /** Effective upstream protocol of the attempted target. */
  upstream_protocol?: string
  /** Lane of the attempt ("native" | "translated"). */
  lane?: string
}

/** Query parameters accepted by `GET /api/logs`. */
export interface LogFilter {
  provider?: string
  model?: string
  real_model?: string
  status_code?: number | string
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

/** Active-target entry returned by `GET /api/active-targets`. */
export interface ActiveTargetEntry {
  model: string
  /**
   * Actual `active_targets` map key — the request model the pin was recorded
   * under. For an exact route this equals `model`; for a wildcard route it is
   * the concrete request model, which is what `DELETE /api/active-targets`
   * must receive to clear the pin. Empty string when the entry is `idle`.
   */
  key: string
  provider: string
  real_model: string
  /**
   * 0-based position of this target in the route's priority-sorted target
   * order (priority descending, ties keep original array order) — the
   * authoritative signal for whether a pin landed on the route's
   * highest-priority (primary) target (index 0) or a fallback. The UI uses
   * it to pick the status-dot color.
   */
  target_index: number
  age_secs: number
  /** "idle" | "pinned" | "expired" */
  status: string
  /** Route strategy: "sticky_failover" | "fill_first" | "round_robin". */
  strategy: string
  /**
   * Position of this target in the cached fill-first shuffled order — `0`
   * means this target currently absorbs all fill-first traffic. `null` when
   * the route is not fill-first or no order is cached yet.
   */
  fill_first_position: number | null
  /** Seconds since the fill-first order was last shuffled. `null` when none. */
  fill_first_cache_age_secs: number | null
  /** `shuffle_cache_ttl_secs` config; `0` = never expire. `null` when none. */
  fill_first_cache_ttl_secs: number | null
}

// ═══════════════════════════════════════════════════════════════════
// Analytics v2 — unified summary / trend query API
// (`GET /v1/analytics/v2/summary|trend`)
// ═══════════════════════════════════════════════════════════════════

/**
 * Statistical scope: route-level final results (one per request) or
 * target-level actual attempts (one per upstream call).
 */
export type AnalyticsScope = "route" | "target"

/**
 * Group key of a v2 summary/trend group. Route scope keys by the stable
 * business identity `route_name`; target scope by the `(provider,
 * real_model)` tuple. UUID identities (`route_id` / `target_id`) are
 * never exposed by the API.
 */
export type AnalyticsGroupKey = { route_name: string } | { provider: string; real_model: string }

/** Latency statistics of one dimension (e2e / ttft / upstream). */
export interface LatencyMetric {
  /** Valid (non-NULL) samples. */
  samples: number
  /** Coverage denominator (the metric's sample universe). */
  population: number
  /** `samples / population`; `null` when the population is 0. */
  coverage: number | null
  /** Arithmetic mean over valid samples, ms. */
  avg_ms: number | null
  /** Nearest-rank 50th percentile, ms. */
  p50_ms: number | null
  /** Nearest-rank 95th percentile, ms. */
  p95_ms: number | null
  /** Nearest-rank 99th percentile, ms. */
  p99_ms: number | null
  /** `true` when `samples < 100` (spec: 有效样本 < 100 → low_sample). */
  low_sample: boolean
  /**
   * `true` when the percentiles were estimated from the 1h rollup
   * histogram (≈1% value error) instead of exact raw samples — i.e. the
   * block spans rollup (historical) data. Backend serializes it only when
   * `true`, so it is absent for exact-detail queries.
   */
  approx_percentiles?: boolean
}

/**
 * One aggregated metric block (`overall` or per-group). Rate fields are
 * `null` when their denominator is 0 — never fabricated as 0.
 */
export interface MetricSummary {
  /** Route scope: `request_results` rows in the span. Target scope: 0. */
  requests: number
  /** Target scope: `target_attempts` rows in the span. Route scope: 0. */
  attempts: number
  /** Final-success samples (route and target scopes). */
  success: number
  /** Route scope statuses (target scope keeps these at 0). */
  client_error: number
  no_route: number
  all_disabled: number
  model_failure: number
  client_cancelled: number
  /** Target scope attempt statuses (route scope keeps these at 0). */
  attempt_failure: number
  attempt_cancelled: number
  /** Non-success count (requests − success, or attempts − success). */
  failure: number
  /** 整体失败率 = non-success / all. */
  overall_failure_rate: number | null
  /** 客户端错误率 = client_error / all requests (route scope only). */
  client_error_rate: number | null
  /** 路由/配置失败率 = (no_route + all_disabled) / (all − client_error). */
  route_config_failure_rate: number | null
  /** 模型失败率 = model_failure / (success + model_failure). */
  model_failure_rate: number | null
  /** 成功率 (denominator differs between overall and group blocks). */
  success_rate: number | null
  /** Token sums over final-success samples only. */
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
  /** Rates per wall-clock second of the query span. */
  request_throughput_per_sec: number | null
  input_token_throughput_per_sec: number | null
  output_token_throughput_per_sec: number | null
  /**
   * Cache statistics over success samples. `cache_known` = samples with
   * non-NULL `cached_tokens` (the hit-rate denominator); `cache_unknown` =
   * success samples with NULL `cached_tokens` (unknown ≠ miss).
   */
  cache_known: number
  cache_hit: number
  cache_miss: number
  cache_unknown: number
  /** 请求命中率 = cache_hit / cache_known; unknown samples are not counted. */
  request_cache_hit_rate: number | null
  /** Token 命中率 = Σ COALESCE(cached_tokens, 0) / Σ prompt_tokens over ALL success samples. */
  token_cache_hit_rate: number | null
  /** Σ prompt_tokens over all success samples (token hit-rate denominator). */
  cache_prompt_tokens: number
  /** Σ COALESCE(cached_tokens, 0) over all success samples (token hit-rate numerator). */
  cache_tokens: number
  /** 端到端 / 上游 / TTFT 延迟 (各自独立 coverage). */
  e2e: LatencyMetric
  ttft: LatencyMetric
  upstream: LatencyMetric
}

/** An attributable group's summary block. */
export interface AnalyticsGroupSummary {
  /** Route scope: `{ route_name }`; target scope: `{ provider, real_model }`. */
  key: AnalyticsGroupKey
  metrics: MetricSummary
}

/** A failure category that cannot be attributed to any group. */
export interface UnassignedFailure {
  /** `final_status` category, e.g. `no_route` / `client_error`. */
  category: string
  count: number
}

/** Response of `GET /v1/analytics/v2/summary`. */
export interface AnalyticsSummaryResponse {
  scope: AnalyticsScope
  /** Left-closed interval start, unix ms. */
  from_ms: number
  /** Right-open interval end, unix ms. */
  to_ms: number
  /** Metrics across the whole span (all statuses / all attempts). */
  overall: MetricSummary
  /** Per-group metrics, sorted by population descending. */
  groups: AnalyticsGroupSummary[]
  /** Unassigned failure categories (route scope; empty for target scope). */
  unassigned_failures: UnassignedFailure[]
}

/** One point of a group trend series (one UTC hour). */
export interface AnalyticsTrendPoint {
  /** UTC hour start, unix ms. */
  hour_ms: number
  /** Route scope: matched non-client-error requests; target scope: actual attempts. */
  requests: number
  /** Final-success samples of this group/hour. */
  success: number
  /** Token sums over success samples of this group/hour. */
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
}

/** Per-hour metric bucket of a trend query (UTC hour, completion time). */
export interface AnalyticsTrendBucket {
  /** UTC hour start, unix ms. */
  hour_ms: number
  /** Metrics of this hour (zero for empty hours). */
  overall: MetricSummary
}

/** One per-group time series of a trend query (zero-filled points). */
export interface AnalyticsTrendSeries {
  key: AnalyticsGroupKey
  /** Sum of `requests` over all points (Top-N trimming order). */
  total_requests: number
  /** One point per UTC hour of the span, in hour order. */
  points: AnalyticsTrendPoint[]
}

/** Aggregation of the series trimmed beyond `top` (absent when none). */
export interface AnalyticsOthersSeries {
  /** Σ `total_requests` of the trimmed series. */
  total_requests: number
  /** Per-hour sums of requests / success / tokens across trimmed series. */
  points: AnalyticsTrendPoint[]
}

/** Response of `GET /v1/analytics/v2/trend` (Top-N + `others`). */
export interface AnalyticsTrendResponse {
  scope: AnalyticsScope
  from_ms: number
  to_ms: number
  /** One bucket per UTC hour of the span, zero-filled. */
  buckets: AnalyticsTrendBucket[]
  /** The `top` leading series by total_requests descending. */
  series: AnalyticsTrendSeries[]
  /** Aggregation of every series beyond `top` (absent when nothing trimmed). */
  others?: AnalyticsOthersSeries
}

/**
 * Query parameters of the v2 summary / trend endpoints. `from` / `to` are
 * required unix-ms timestamps (left-closed right-open interval). Spans
 * ≤ 24h query the exact-detail kernel; longer spans (e.g. 7d) use the
 * mixed path — 1h rollup tables for the history, exact details for the
 * recent 24h (historical latency percentiles are then approximate, see
 * `LatencyMetric.approx_percentiles`).
 */
export interface AnalyticsV2Query {
  /** Required: interval start, unix ms (left-closed). */
  from: number
  /** Required: interval end, unix ms (right-open). */
  to: number
  /** `route` (default) or `target`. */
  scope?: AnalyticsScope
  /** Optional route_name filter (both scopes; empty = no filter). */
  route?: string
  /** Target scope only: keep attempts from this provider. */
  provider?: string
  /** Target scope only: keep attempts with this real model. */
  real_model?: string
  /** Trend only: leading series to keep (default 20, server cap 50). */
  top?: number
}

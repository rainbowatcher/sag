/**
 * Route `pipeline` 三值的纯逻辑层。
 *
 * 三个 pipeline 共享一个 route 身份（`model_name`），但可写字段不同：
 * - `image` 是唯一允许 `api_endpoint` 的 pipeline；
 * - `systemone` 只服务 `POST /v1/systemone`，因此既不能带 `api_endpoint`，
 *   也不支持 `session_sticky`（后端 `dispatch_systemone` 丢弃 write-back）。
 *
 * 组件只做绑定，互斥字段的显示 / 清空 / payload 形态都收敛在这里，便于单测。
 */
import type { Pipeline, Strategy } from "@/types/api"

/** Every `pipeline` value the backend accepts (serde names, no separators). */
export const PIPELINES = ["chat", "image", "systemone"] as const satisfies readonly Pipeline[]

/** Runtime guard for a value coming from the DOM or the API. */
export function isPipeline(value: unknown): value is Pipeline {
  return typeof value === "string" && (PIPELINES as readonly string[]).includes(value)
}

/** `api_endpoint` only exists for the image pipeline: chat/systemone must omit or null it. */
export function showsApiEndpoint(pipeline: Pipeline): boolean {
  return pipeline === "image"
}

/** `systemone` dispatch performs no session write-back, so pinning it is rejected. */
export function supportsSessionSticky(pipeline: Pipeline): boolean {
  return pipeline !== "systemone"
}

/** Form fields to rewrite when the pipeline switches. Absent key = leave alone. */
export interface PipelineSwitchReset {
  api_endpoint?: ""
  strategy?: Strategy
  session_ttl_secs?: ""
  session_max_entries?: ""
}

/**
 * Fields that must be cleared when the pipeline changes to `to`.
 * `strategy` is the value *before* the switch, because only an actual
 * `session_sticky` selection has to be forced back to a supported strategy.
 */
export function pipelineSwitchReset(to: Pipeline, strategy: Strategy): PipelineSwitchReset {
  const reset: PipelineSwitchReset = {}
  // Leaving image must not keep an endpoint that the backend would reject.
  if (!showsApiEndpoint(to)) reset.api_endpoint = ""
  // A SystemOne route cannot pin sessions, so neither the strategy nor the
  // TTL/capacity inputs may survive the switch.
  if (!supportsSessionSticky(to)) {
    reset.session_ttl_secs = ""
    reset.session_max_entries = ""
    if (strategy === "session_sticky") reset.strategy = "sticky_failover"
  }
  return reset
}

/**
 * Create payload fragment: only image sends `api_endpoint`; chat/systemone
 * omit the key entirely (`CreateRoutePayload` has no `null` form).
 */
export function createEndpointField(pipeline: Pipeline, raw: string): { api_endpoint?: string } {
  return showsApiEndpoint(pipeline) ? { api_endpoint: raw.trim() } : {}
}

/**
 * Update payload fragment: triple-state patch, so a non-image route sends an
 * explicit `null` to clear a previously saved endpoint.
 */
export function updateEndpointField(
  pipeline: Pipeline,
  raw: string,
): { api_endpoint: string | null } {
  return { api_endpoint: showsApiEndpoint(pipeline) ? raw.trim() : null }
}

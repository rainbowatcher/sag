import { computed, onMounted, ref, watch } from "vue"
import type { Ref } from "vue"
import { PROBE_KIND_ORDER, defaultProbeKind } from "@/lib/provider-probe"
import { useProviderStore } from "@/stores/providers"
import type { ApiConfig, ApiKind, ProxyMode } from "@/types/api"

export interface UseProviderProbeOptions {
  /** Initial `apis` catalog (edit form seeds from the saved provider). */
  initialApis?: Partial<Record<ApiKind, ApiConfig>>
  /** Initial `fallback` protocol (edit form seeds from the saved provider). */
  initialFallback?: ApiKind | null
  /** Provider key list owned by the form. */
  apiKeys: Ref<string[]>
  /** Proxy mode owned by the form. */
  proxyMode: Ref<ProxyMode>
  /** Callback into the form's provider-level headers KV state. */
  getHeaders: () => Record<string, string>
}

/**
 * Shared protocol-probe state and actions for the provider add/edit forms.
 *
 * Owns the `apis` catalog, the probe section selection, and the model
 * listing / model test actions. The forms keep ownership of `apiKeys`,
 * `proxyMode`, and the provider-level headers KV state, handing the latter
 * in via `getHeaders`.
 */
export function useProviderProbe(options: UseProviderProbeOptions) {
  const store = useProviderStore()

  onMounted(() => {
    store.upstreamModels = []
    store.upstreamModelsError = ""
  })

  // ── API sections (protocol catalog) ──────────────────────────
  // The editor owns the whole directory: updates always send the full
  // `apis` map (whole-directory replace on the backend).
  const apis = ref<Partial<Record<ApiKind, ApiConfig>>>({ ...options.initialApis })
  const fallback = ref<ApiKind | null>(options.initialFallback ?? null)
  const apisEditor = ref<{ validate: () => string | null } | null>(null)

  /**
   * Default section for model listing / model tests: the `fallback` protocol
   * when declared, else chat_completions, else the next protocol key — never
   * the `image` section, independent of key insertion order.
   */
  const primaryKind = computed<ApiKind | null>(() => defaultProbeKind(apis.value, fallback.value))

  /** Sections selectable as the models-probe target (protocol keys only, fixed order). */
  const protocolSections = computed<ApiKind[]>(() => {
    const keys = Object.keys(apis.value) as ApiKind[]
    return PROBE_KIND_ORDER.filter((k) => keys.includes(k))
  })

  /** Currently selected probe section; falls back to the default when it disappears. */
  const probeKind = ref<ApiKind | null>(null)
  watch(
    [primaryKind, protocolSections],
    () => {
      if (!probeKind.value || !protocolSections.value.includes(probeKind.value)) {
        probeKind.value = primaryKind.value
      }
    },
    { immediate: true },
  )
  const probeApi = computed<ApiConfig | null>(() =>
    probeKind.value ? (apis.value[probeKind.value] ?? null) : null,
  )

  function onFetchModels() {
    const probe = probeApi.value
    if (!probe) {
      store.upstreamModelsError = "Declare an API section first (APIs tab)"
      return
    }
    store.fetchUpstreamModels({
      base_url: probe.base_url,
      api_keys: options.apiKeys.value.filter((k) => k.trim()),
      headers: probe.headers ?? options.getHeaders(),
      api_kind: probeKind.value ?? undefined,
      models_endpoint: probe.models_endpoint,
      proxy_mode: options.proxyMode.value,
    })
  }

  function onTestModel(model: string) {
    const probe = probeApi.value
    if (!probe) {
      store.upstreamModelsError = "Declare an API section first (APIs tab)"
      return
    }
    store.testModelWithState(model, {
      model,
      api_keys: options.apiKeys.value.filter((k) => k.trim()),
      base_url: probe.base_url,
      api_kind: probeKind.value ?? undefined,
      headers: probe.headers ?? options.getHeaders(),
      proxy_mode: options.proxyMode.value,
    })
  }

  return {
    apis,
    fallback,
    apisEditor,
    primaryKind,
    protocolSections,
    probeKind,
    probeApi,
    onFetchModels,
    onTestModel,
  }
}

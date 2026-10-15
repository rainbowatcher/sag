import { en } from "./en.ts"

/**
 * Chinese copy. The `Record<keyof typeof en, string>` annotation makes a
 * missing or extra key a compile-time error.
 */
export const zh: Record<keyof typeof en, string> = {
  // ── Shared vocabulary ───────────────────────────────────────────
  "common.save": "保存",
  "common.cancel": "取消",
  "common.confirm": "确认",
  "common.delete": "删除",
  "common.edit": "编辑",
  "common.close": "关闭",
  "common.refresh": "刷新",
  "common.loading": "加载中...",
  "common.retry": "重试",
  "common.expired": "已过期",
  "common.enabled": "已启用",
  "common.disabled": "已禁用",
  "common.actions": "操作",
  "common.status": "状态",
  "common.name": "名称",
  "common.description": "描述",
  "common.provider": "供应商",
  "common.model": "模型",
  "common.yes": "是",
  "common.no": "否",

  // ── Navigation ────────────────────────────────────────────────
  "nav.dashboard": "仪表盘",
  "nav.providers": "供应商",
  "nav.routes": "路由",
  "nav.analytics": "分析",
  "nav.logs": "日志",
  "nav.settings": "设置",

  // ── Settings › Language ─────────────────────────────────────────
  "settings.language.tab": "语言",
  "settings.language.title": "语言",
  "settings.language.description": "选择管理后台的显示语言。该设置保存在当前浏览器中。",
  "settings.language.label": "界面语言",
  "settings.language.optionEnglish": "English",
  "settings.language.optionChinese": "中文",

  // ── Circuit breakers ────────────────────────────────────────────
  "circuitBreaker.recovered": "恢复 {provider}/{model} — 下一个请求将直接打上游",
  "circuitBreaker.restoreFailed": "恢复失败",
  "circuitBreaker.forceCloseTitle": "强制恢复该 target，下一个请求将直接打上游",
  "circuitBreaker.restore": "恢复",
  "circuitBreaker.cooldownElapsed": "已可恢复",
  "circuitBreaker.probeElapsed": "已可探测",

  // ── Providers ───────────────────────────────────────────────────
  "providers.addForm.timeoutPlaceholder": "600 (默认)",
  "providers.advanced.bodyOverrideWarningTitle": "提示",
  "providers.advanced.bodyOverrideWarningLead": "Body Overrides 对该供应商的",
  "providers.advanced.bodyOverrideWarningAllModels": "所有模型",
  "providers.advanced.bodyOverrideWarningAfterAllModels":
    "生效。如需为特定模型设置不同参数，请前往 ",
  "providers.advanced.bodyOverrideWarningApisTab": "APIs Tab",
  "providers.advanced.bodyOverrideWarningAfterApisTab": " 对应 API section 的 Advanced 中配置 ",
  "providers.advanced.bodyOverrideWarningModelOverrides": "Model Overrides",
  "providers.advanced.bodyOverrideWarningAfterModelOverrides":
    "，避免在多个路由的 target 中重复配置 ",
  "providers.advanced.bodyOverrideWarningEnd": "。",

  // ── Analytics ───────────────────────────────────────────────────
  "analytics.approxPercentilesTooltip": "历史区间由 1h Rollup 直方图估算（≈1% 误差），非精确分位数",
  "analytics.cacheUnknownTooltip":
    "成功样本中未上报 cached_tokens 的数量（unknown ≠ miss，不计入命中率分母）",
  "analytics.overallOnlyMetric": "该指标仅整体线可用，无逐组明细",

  // ── Providers: toasts ────────────────────────────────────────
  "providers.toast.created": '供应商 "{name}" 已创建',
  "providers.toast.updated": '供应商 "{name}" 已更新',
  "providers.toast.deleted": '供应商 "{name}" 已删除',
  "providers.toast.enabled": '供应商 "{name}" 已启用',
  "providers.toast.disabled": '供应商 "{name}" 已禁用',
  "providers.toast.copyFailed": "复制供应商失败",

  // ── Providers: form validation ───────────────────────────────
  "providers.form.nameRequired": "名称不能为空",
  "providers.form.apiKeyRequired": "至少需要一个 API key",
  "providers.form.bodyOverrideInvalidJson": 'Body override "{key}"：JSON 值无效',

  // ── Providers: list table ────────────────────────────────────
  "providerList.columns.apis": "APIs",
  "providerList.columns.keys": "Keys",
  "providerList.columns.cb": "熔断",
  "providerList.allTargetsHealthy": "所有 target 均健康",
  "providerList.breakerOpenGroup": "Open ({count})：",
  "providerList.breakerHalfOpenGroup": "Half-Open ({count})：",
  "providerList.enableProvider": "启用供应商",
  "providerList.disableProvider": "禁用供应商",
  "providerList.editProvider": "编辑供应商",
  "providerList.copyProvider": "复制供应商",
  "providerList.deleteProvider": "删除供应商",
  "providerList.cancelDelete": "取消删除",
  "providerList.fallbackAuto": "fallback: auto",
  "providerList.fallbackNamed": "fallback: {name}",

  // ── Routes: toasts ───────────────────────────────────────────
  "routes.toast.created": '路由 "{name}" 已创建',
  "routes.toast.updated": '路由 "{name}" 已更新',
  "routes.toast.deleted": '路由 "{name}" 已删除',
  "routes.toast.createFailed": "创建路由失败",
  "routes.toast.fillFirstReset": '已重置 "{model}" 的 fill-first 顺序，下个请求将重新洗牌',
  "routes.toast.pinCleared": '已清除 "{model}" 的 sticky pin，将回退到优先级路由',
  "routes.toast.clearActiveFailed": "清除 active target 失败",

  // ── Routes: form validation ──────────────────────────────────
  "routes.form.apiEndpointRequired": "image pipeline 需要 API endpoint",
  "routes.form.targetRequired": "至少需要一个 target",
  "routes.form.targetProviderRequired": "每个 target 都必须选择 provider",
  "routes.form.modelNameRequired": "请填写 model_name，或为第一个 target 填写 real_model",
  "routes.form.targetExtra": "Target {name}：{error}",

  // ── Routes: list table ───────────────────────────────────────
  "routeList.columns.modelName": "模型名称",
  "routeList.columns.pipeline": "Pipeline",
  "routeList.columns.strategy": "策略",
  "routeList.columns.targets": "Targets",
  "routeList.columns.activeTarget": "Active Target",
  "routeList.allTargetsUnavailable": "所有 target 均不可用",
  "routeList.targetsValid": "{valid}/{total} 个 target 有效",
  "routeList.targetMissingProvider": 'Target #{index}：未找到供应商 "{provider}"',
  "routeList.targetProviderDisabled": 'Target #{index}：供应商 "{provider}" 已禁用',
  "routeList.targetModelDisabled": 'Target #{index}：模型 "{model}" 未在供应商 "{provider}" 启用',
  "routeList.tooltip.orderAge": "顺序时长",
  "routeList.tooltip.pinAge": "Pin 时长",
  "routeList.tooltip.action": "操作",
  "routeList.fillFirstTarget": "Fill-first target",
  "routeList.clickToReset": "点击重置",
  "routeList.clickToClear": "点击清除",
  "routeList.editRoute": "编辑路由",
  "routeList.deleteRoute": "删除路由",

  // ── Analytics › V2 panel ─────────────────────────────────────
  "analytics.v2.columns.actualModel": "实际模型",
  "analytics.v2.columns.attempts": "尝试次数",
  "analytics.v2.columns.success": "成功",
  "analytics.v2.columns.failure": "失败",
  "analytics.v2.columns.successRate": "成功率",
  "analytics.v2.columns.avgE2e": "平均 E2E",
  "analytics.v2.columns.p95E2e": "P95 E2E",
  "analytics.v2.columns.tokensIn": "输入 Tokens",
  "analytics.v2.columns.tokensOut": "输出 Tokens",
  "analytics.v2.columns.cacheHit": "缓存命中",
  "analytics.v2.columns.cacheUnknown": "缓存未知",
  "analytics.v2.columns.routeName": "路由名称",
  "analytics.v2.columns.requests": "请求数",
  "analytics.v2.columns.modelFailure": "模型失败",
  "analytics.v2.others": "其他",
  "analytics.v2.card.e2e": "E2E",
  "analytics.v2.card.ttft": "TTFT",
  "analytics.v2.card.upstream": "上游",

  // ── Settings: toasts ─────────────────────────────────────────
  "settings.toast.circuitBreakerSaved": "熔断设置已保存",
  "settings.toast.backoffSaved": "退避设置已保存",
  "settings.toast.strategyTimersSaved": "策略计时器已保存",
  "settings.toast.proxySaved": "代理设置已保存",
  "settings.toast.proxyCleared": "代理设置已清除",
  "settings.toast.apiKeysSaved": "API keys 已保存",
  "settings.toast.authDisabled": "认证已关闭",

  // ── Settings: proxy / API-key validation ─────────────────────
  "settings.proxy.invalidUrl": "URL 无效。请使用 http://host:port 或 https://host:port 格式。",
  "settings.proxy.httpOnly": "仅支持 HTTP/HTTPS 代理 URL（不支持 SOCKS）。",
  "settings.apiKeys.emptyOrWhitespace": "API key 不能为空或仅含空白字符。",
  "settings.apiKeys.emptyRows": "保存前请删除空白 key 行，或将其填写完整。",

  // ── Settings: read-only info rows ────────────────────────────
  "settings.info.host": "主机",
  "settings.info.port": "端口",
  "settings.info.globalProxy": "全局代理",
  "settings.info.direct": "直连（无代理）",
  "settings.info.level": "级别",
  "settings.info.format": "格式",
  "settings.info.enabled": "启用",
  "settings.info.storageDsn": "存储 DSN",

  // ── Settings: numeric field validation ───────────────────────
  "settings.form.secsMin1": "{label} 必须是 ≥ 1 的整数秒。",
  "settings.form.secsMin0": "{label} 必须是 ≥ 0 的整数秒。",
  "settings.form.secsMax": "{label} 不得超过 {max} 秒（一年）。",
  "settings.circuitBreaker.cooldownQuota": "配额耗尽冷却",
  "settings.circuitBreaker.cooldownRate": "限流冷却",
  "settings.circuitBreaker.cooldownGeneric": "通用冷却",
  "settings.circuitBreaker.probeInterval": "探测间隔",
  "settings.strategyTimers.fillFirstTtl": "Fill-first 缓存 TTL",
  "settings.strategyTimers.stickyReset": "Sticky failover 重置",
  "settings.backoff.retriesMin": "重试次数至少为 1。",
  "settings.backoff.baseNonNegative": "基础延迟必须为非负数。",
  "settings.backoff.maxNonNegative": "最大延迟必须为非负数。",
  "settings.backoff.baseExceedsMax": "基础延迟不能超过最大延迟。",
}

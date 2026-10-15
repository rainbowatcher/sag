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
}

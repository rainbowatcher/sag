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
}

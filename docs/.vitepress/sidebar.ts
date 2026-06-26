import fs from 'node:fs'
import path from 'node:path'

/** 模块目录名 → 侧边栏显示名。不在此表中的模块不会出现在侧边栏。 */
const MODULE_LABELS: Record<string, string> = {
  architecture: '架构',
  config: '配置',
  routing: '路由',
  dispatch: '调度',
  providers: '供应商',
  'circuit-breaker': '熔断器',
  auth: '认证',
  analytics: '分析',
  logging: '日志',
  admin: '管理 API',
}

/** 默认展开的模块（在侧边栏中 collapsed: false）。 */
const EXPANDED = new Set(['architecture', 'routing'])

/** 从 filesystem 扫描 docs/spec/ 目录，生成 VitePress sidebar 数组。 */
export function generateSpecSidebar(specDir: string) {
  const items: any[] = []

  const entries = fs.readdirSync(specDir, { withFileTypes: true })
  for (const entry of entries) {
    if (!entry.isDirectory()) continue
    const name = entry.name
    const label = MODULE_LABELS[name]
    if (!label) continue

    const dir = path.join(specDir, name)
    const hasReq = fs.existsSync(path.join(dir, 'requirements.md'))
    const hasSpec = fs.existsSync(path.join(dir, 'spec.md'))
    if (!hasReq && !hasSpec) continue

    const sub: any[] = []
    if (hasReq) sub.push({ text: '需求', link: `/spec/${name}/requirements` })
    if (hasSpec) sub.push({ text: '规格', link: `/spec/${name}/spec` })

    items.push({
      text: label,
      collapsed: !EXPANDED.has(name),
      items: sub,
    })
  }

  // 变更时间线置底
  if (fs.existsSync(path.join(specDir, 'CHANGELOG.md'))) {
    items.push({ text: '变更时间线', link: '/spec/CHANGELOG' })
  }

  return items
}

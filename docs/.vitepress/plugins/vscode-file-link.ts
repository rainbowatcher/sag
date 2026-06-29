import type { Plugin } from 'vitepress'
import path from 'node:path'

export interface VscodeFileLinkOptions {
  repoRoot: string
}

/**
 * 匹配 Markdown 中的相对路径链接：[...](<relative-path>)
 * 排除图片 ![...](...) 和锚点 / 协议链接
 */
const RELATIVE_LINK_RE =
  /(?<!\!)\[([^\]]*)\]\(((?:\.\.\/|\.\/)[^\s)]+)(?:\s+"[^"]*")?\)/g

export function vscodeFileLinkPlugin(opts: VscodeFileLinkOptions): Plugin {
  const { repoRoot } = opts

  return {
    name: 'vscode-file-link',
    enforce: 'pre',
    transform(code: string, id: string) {
      // 仅 dev 模式生效
      if (process.env.NODE_ENV !== 'development') return
      if (!id.endsWith('.md')) return

      const mdDir = path.dirname(id) // id is absolute path in Vite

      const newCode = code.replace(
        RELATIVE_LINK_RE,
        (match, text, url) => {
          const resolved = path.resolve(mdDir, url)
          // 只转换指向仓库内文件的链接
          if (!resolved.startsWith(repoRoot)) return match
          return `[${text}](vscode://file/${resolved})`
        },
      )

      return { code: newCode, map: null }
    },
  }
}

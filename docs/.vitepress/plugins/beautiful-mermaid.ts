import type { Plugin } from 'vitepress'
import { renderMermaidSVG } from 'beautiful-mermaid'

const renderCache = new Map<string, string>()

function injectMermaidThemeVars(svg: string): string {
  const styleVars =
    '--bg:var(--m-bg,#eceff4);' +
    '--fg:var(--m-fg,#2e3440);' +
    '--accent:var(--m-accent,#5e81ac);' +
    '--surface:var(--m-surface,#d8dee9);' +
    '--border:var(--m-border,#81a1c1);' +
    '--line:var(--m-line,#81a1c1);' +
    '--muted:var(--m-muted,#60728a)'

  const openStart = svg.indexOf('<svg')
  if (openStart < 0) return svg
  const openEnd = svg.indexOf('>', openStart)
  if (openEnd < 0) return svg
  const openTag = svg.slice(openStart, openEnd + 1)

  const styleRe = /\sstyle="[^"]*"/
  const newOpenTag = styleRe.test(openTag)
    ? openTag.replace(styleRe, ` class="mermaid-svg" style="${styleVars}"`)
    : openTag.replace(/^<svg/, `<svg class="mermaid-svg" style="${styleVars}"`)

  return newOpenTag + svg.slice(openEnd + 1)
}

export function beautifulMermaidPlugin(): Plugin {
  return {
    name: 'beautiful-mermaid',
    enforce: 'pre',
    transform(code: string, id: string) {
      if (!id.endsWith('.md')) return

      const mermaidRegex = /```mermaid\r?\n([\s\S]*?)```/g
      const newCode = code.replace(mermaidRegex, (match, source) => {
        const trimmed = source.replace(/\r\n?/g, '\n').trim()

        if (renderCache.has(trimmed)) {
          return renderCache.get(trimmed)!
        }

        try {
          const svg = renderMermaidSVG(trimmed, {
            bg: '#eceff4',
            fg: '#2e3440',
            accent: '#5e81ac',
            surface: '#d8dee9',
            border: '#81a1c1',
            line: '#81a1c1',
            muted: '#60728a',
            transparent: true,
            font: 'Inter, system-ui, sans-serif',
          })
          const themed = injectMermaidThemeVars(svg)
          const b64 = Buffer.from(themed, 'utf-8').toString('base64')
          const wrapped = `<MermaidRenderer svg="${b64}" />`
          renderCache.set(trimmed, wrapped)
          return wrapped
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err)
          console.error(`[beautiful-mermaid] render failed: ${msg}`)
          return `<div class="mermaid-error"><pre>${msg}</pre></div>`
        }
      })

      return { code: newCode, map: null }
    },
  }
}

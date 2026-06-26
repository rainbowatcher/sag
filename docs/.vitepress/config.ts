import { defineConfig } from 'vitepress'
import { beautifulMermaidPlugin } from './plugins/beautiful-mermaid'
import { generateSpecSidebar } from './sidebar'
import path from 'node:path'

const specDir = path.resolve(__dirname, '../spec')

export default defineConfig({
  title: 'SAG Documentation',
  description: 'Smart AI Gateway 文档',
  base: '/',
  lang: 'zh-CN',
  head: [
    ['link', { rel: 'icon', href: '/favicon.svg' }],
  ],
  themeConfig: {
    logo: '/favicon.svg',
    siteTitle: 'SAG',

    // ── Navbar — two top-level entry points ──────────────────────────
    nav: [
      { text: '使用指南', link: '/guide/' },
      { text: '技术规格', link: '/spec/' },
    ],

    // ── Sidebar — path-based, separate for /guide/ and /spec/ ───────
    sidebar: {
      '/guide/': [
        { text: '使用指南', link: '/guide/' },
        { text: '配置', link: '/guide/configuration' },
        { text: '路由', link: '/guide/routing' },
        { text: '供应商', link: '/guide/providers' },
        { text: '部署', link: '/guide/deployment' },
        {
          text: 'API 参考',
          collapsed: false,
          items: [
            { text: '模型 API', link: '/guide/models-api' },
            { text: '管理 API', link: '/guide/admin-api' },
          ],
        },
        { text: '分析', link: '/guide/analytics' },
      ],

      '/spec/': [
        { text: '技术规格', link: '/spec/' },
        ...generateSpecSidebar(specDir),
      ],
    },

    socialLinks: [
      { icon: 'github', link: 'http://localhost:30001/rainbowatcher/sag.git' },
    ],
    editLink: {
      pattern: 'http://localhost:30001/rainbowatcher/sag/edit/main/docs/:path',
      text: '在 GitHub 上编辑此页',
    },
    search: { provider: 'local' },
  },
  ignoreDeadLinks: true,
  markdown: {
    lineNumbers: true,
  },
  vite: {
    plugins: [beautifulMermaidPlugin()],
  },
})

import { defineConfig } from 'vitepress'
import { beautifulMermaidPlugin } from './plugins/beautiful-mermaid'

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
    siteTitle: 'SAG Documentation',
    sidebar: [
      { text: '概述', link: '/' },
      {
        text: '概念',
        items: [
          { text: '架构', link: '/architecture' },
          { text: '配置', link: '/configuration' },
          { text: '供应商', link: '/providers' },
          { text: '路由', link: '/routing' },
          { text: '分析', link: '/analytics' },
        ],
      },
      {
        text: 'API 参考',
        items: [
          { text: '模型 API', link: '/models-api' },
          { text: '管理 API', link: '/admin-api' },
        ],
      },
      { text: '部署', link: '/deployment' },
    ],
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

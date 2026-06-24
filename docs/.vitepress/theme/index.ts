import { h } from 'vue'
import type { Theme } from 'vitepress'
import DefaultTheme from 'vitepress/theme'
import MermaidRenderer from './components/MermaidRenderer.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  Layout: () => h(DefaultTheme.Layout, null, {}),
  enhanceApp({ app }) {
    app.component('MermaidRenderer', MermaidRenderer)
  },
} satisfies Theme

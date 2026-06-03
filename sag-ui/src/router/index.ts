import { createRouter, createWebHistory } from 'vue-router'
import type { RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
  {
    path: '/',
    component: () => import('@/components/AppLayout.vue'),
    children: [
      {
        path: '',
        name: 'dashboard',
        component: () => import('@/pages/DashboardPage.vue'),
      },
      {
        path: 'providers',
        name: 'providers',
        component: () => import('@/pages/ProvidersPage.vue'),
      },
      {
        path: 'routes',
        name: 'routes',
        component: () => import('@/pages/RoutesPage.vue'),
      },
      {
        path: 'analytics',
        name: 'analytics',
        component: () => import('@/pages/AnalyticsPage.vue'),
      },
    ],
  },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

export default router

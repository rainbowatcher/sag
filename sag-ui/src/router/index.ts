import { createRouter, createWebHistory } from "vue-router"
import type { RouteRecordRaw } from "vue-router"
import { useConnectionStore } from "@/stores/connection"

const routes: RouteRecordRaw[] = [
  {
    path: "/connect",
    name: "connect",
    component: () => import("@/pages/ConnectPage.vue"),
  },
  {
    path: "/",
    component: () => import("@/components/AppLayout.vue"),
    children: [
      {
        path: "",
        name: "dashboard",
        component: () => import("@/pages/DashboardPage.vue"),
      },
      {
        path: "providers",
        name: "providers",
        component: () => import("@/pages/ProvidersPage.vue"),
      },
      {
        path: "routes",
        name: "routes",
        component: () => import("@/pages/RoutesPage.vue"),
      },
      {
        path: "analytics",
        name: "analytics",
        component: () => import("@/pages/AnalyticsPage.vue"),
      },
      {
        path: "analytics",
        name: "analytics",
        component: () => import("@/pages/AnalyticsPage.vue"),
      },
      {
        path: "logs",
        name: "logs",
        component: () => import("@/pages/LogsPage.vue"),
      },
    ],
  },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

// Wire up the router reference in the store so 401 handlers can redirect
router.isReady().then(() => {
  const store = useConnectionStore()
  store.setRouter(router)
})

// Navigation guard: auto-connect or redirect to /connect
router.beforeEach(async (to) => {
  // Allow access to /connect unconditionally
  if (to.path === "/connect") return true

  const store = useConnectionStore()

  // Already connected and validated → proceed
  if (store.isConnected) return true

  // Try to load saved connection
  const loaded = store.loadConnection()
  if (!loaded) return "/connect"

  // Validate saved connection silently
  const ok = await store.validateConnection()
  if (!ok) return "/connect?reason=expired"

  return true
})

export default router

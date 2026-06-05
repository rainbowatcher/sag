import { createRouter, createWebHistory } from 'vue-router';
import { useApiStore } from '@/stores/api';

const routes = [
    {
        path: '/connect',
        name: 'connect',
        component: () => import('@/pages/ConnectPage.vue'),
    },
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
];

const router = createRouter({
    history: createWebHistory(),
    routes,
});

router.isReady().then(() => {
    const store = useApiStore();
    store.setRouter(router);
});

router.beforeEach(async (to) => {
    if (to.path === '/connect') return true;

    const store = useApiStore();

    if (store.isConnected) return true;

    const loaded = store.loadConnection();
    if (!loaded) return '/connect';

    const ok = await store.validateConnection();
    if (!ok) return '/connect?reason=expired';

    return true;
});

export default router;

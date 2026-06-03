import { defineStore } from 'pinia';
import { ref, computed } from 'vue';
import axios from 'axios';
const api = axios.create({ baseURL: '' });
export const useApiStore = defineStore('api', () => {
    const apiKey = ref('');
    function headers() {
        return apiKey.value ? { Authorization: `Bearer ${apiKey.value}` } : {};
    }
    // ── Providers ──
    const providers = ref([]);
    const providersLoading = ref(false);
    async function fetchProviders() {
        providersLoading.value = true;
        try {
            const { data } = await api.get('/api/providers', { headers: headers() });
            providers.value = data.providers;
        }
        finally {
            providersLoading.value = false;
        }
    }
    async function addProvider(payload) {
        await api.post('/api/providers', payload, { headers: headers() });
    }
    // ── Routes ──
    const routes = ref([]);
    const routesLoading = ref(false);
    async function fetchRoutes() {
        routesLoading.value = true;
        try {
            const { data } = await api.get('/api/routes', { headers: headers() });
            routes.value = data.routes;
        }
        finally {
            routesLoading.value = false;
        }
    }
    async function addRoute(providerName, payload) {
        await api.post(`/api/providers/${providerName}/routes`, payload, { headers: headers() });
    }
    // ── Health ──
    const gatewayVersion = ref('');
    const gatewayOnline = ref(false);
    async function checkHealth() {
        try {
            const { data } = await api.get('/health');
            gatewayOnline.value = data.status === 'ok';
            gatewayVersion.value = data.version;
        }
        catch {
            gatewayOnline.value = false;
        }
    }
    // ── Analytics ──
    const stats = ref([]);
    const statsLoading = ref(false);
    async function fetchStats(provider, model) {
        statsLoading.value = true;
        try {
            const params = new URLSearchParams();
            if (provider)
                params.set('provider', provider);
            if (model)
                params.set('model', model);
            const { data } = await api.get(`/v1/analytics/stats?${params}`);
            stats.value = data.data;
        }
        finally {
            statsLoading.value = false;
        }
    }
    // ── Summary ──
    const summary = computed(() => ({
        providerCount: providers.value.length,
        routeCount: routes.value.length,
        totalCalls: stats.value.reduce((s, r) => s + r.call_count, 0),
        totalErrors: stats.value.reduce((s, r) => s + r.error_count, 0),
    }));
    return {
        apiKey,
        providers, providersLoading, fetchProviders, addProvider,
        routes, routesLoading, fetchRoutes, addRoute,
        gatewayVersion, gatewayOnline, checkHealth,
        stats, statsLoading, fetchStats,
        summary,
    };
});

import { defineStore } from 'pinia';
import { ref, computed } from 'vue';
import axios from 'axios';
const api = axios.create({ baseURL: '' });
const LS_KEY = 'sag-connection';
const TTL_MS = 30 * 24 * 60 * 60 * 1000; // 30 days
export const useApiStore = defineStore('api', () => {
    // ═══════════════════════════════════════════════════════════════
    // Connection state (persisted to localStorage)
    // ═══════════════════════════════════════════════════════════════
    const connection = ref(null);
    const validated = ref(false);
    const connecting = ref(false);
    const connectError = ref('');
    const isConnected = computed(() => connection.value !== null && validated.value);
    const gatewayUrl = computed(() => connection.value?.gatewayUrl ?? '');
    function headers() {
        return connection.value?.apiKey
            ? { Authorization: `Bearer ${connection.value.apiKey}` }
            : {};
    }
    function loadConnection() {
        const raw = localStorage.getItem(LS_KEY);
        if (!raw)
            return false;
        let conn;
        try {
            conn = JSON.parse(raw);
        }
        catch {
            localStorage.removeItem(LS_KEY);
            return false;
        }
        if (Date.now() - conn.savedAt > TTL_MS) {
            localStorage.removeItem(LS_KEY);
            return false;
        }
        connection.value = conn;
        api.defaults.baseURL = conn.gatewayUrl;
        return true;
    }
    function saveConnection(url, key) {
        const conn = { gatewayUrl: url, apiKey: key, savedAt: Date.now() };
        localStorage.setItem(LS_KEY, JSON.stringify(conn));
        connection.value = conn;
        api.defaults.baseURL = url;
    }
    async function connect(url, key) {
        connecting.value = true;
        connectError.value = '';
        // Ensure URL has a protocol
        let normalizedUrl = url;
        if (!/^https?:\/\//i.test(normalizedUrl)) {
            normalizedUrl = 'http://' + normalizedUrl;
        }
        try {
            // Step 1: Health check (public endpoint)
            const healthApi = axios.create({ baseURL: normalizedUrl });
            let healthOk = false;
            try {
                const { data } = await healthApi.get('/health', { timeout: 5000 });
                healthOk = data.status === 'ok';
                if (healthOk) {
                    gatewayVersion.value = data.version;
                    gatewayOnline.value = true;
                }
            }
            catch {
                // continue — will be reported below
            }
            if (!healthOk) {
                const msg = 'Cannot reach gateway. Check the URL and ensure the gateway is running.';
                connectError.value = msg;
                return msg;
            }
            // Step 2: Auth check — call /api/providers with the key
            const authApi = axios.create({ baseURL: normalizedUrl });
            let authOk = false;
            try {
                const { data } = await authApi.get('/api/providers', {
                    headers: { Authorization: `Bearer ${key}` },
                    timeout: 5000,
                });
                providers.value = data.providers;
                authOk = true;
            }
            catch (e) {
                if (e.response?.status === 401) {
                    const msg = 'Invalid API key — please check and re-enter.';
                    connectError.value = msg;
                    return msg;
                }
                const msg = `Unexpected error: ${e.message}`;
                connectError.value = msg;
                return msg;
            }
            if (!authOk) {
                const msg = 'Auth check failed.';
                connectError.value = msg;
                return msg;
            }
            // Success
            saveConnection(normalizedUrl, key);
            validated.value = true;
            connectError.value = '';
            // Preload routes (awaited to avoid race with dashboard render)
            await fetchRoutes();
            return null; // null = success
        }
        finally {
            connecting.value = false;
        }
    }
    function disconnect() {
        localStorage.removeItem(LS_KEY);
        connection.value = null;
        validated.value = false;
        api.defaults.baseURL = '';
        providers.value = [];
        routes.value = [];
    }
    const validating = ref(false);
    async function validateConnection() {
        validating.value = true;
        try {
            const { data } = await api.get('/health', { timeout: 3000 });
            if (data.status === 'ok') {
                validated.value = true;
                gatewayOnline.value = true;
                gatewayVersion.value = data.version ?? gatewayVersion.value;
                return true;
            }
        }
        catch {
            gatewayOnline.value = false;
            return false;
        }
        finally {
            validating.value = false;
        }
        gatewayOnline.value = false;
        return false;
    }
    // ═══════════════════════════════════════════════════════════════
    // 401 handler — redirect to connect screen
    // ═══════════════════════════════════════════════════════════════
    let _router = null;
    function setRouter(router) {
        _router = router;
    }
    function handle401() {
        connection.value = null;
        validated.value = false;
        api.defaults.baseURL = '';
        providers.value = [];
        routes.value = [];
        if (_router) {
            _router.push('/connect?reason=expired');
        }
    }
    // ═══════════════════════════════════════════════════════════════
    // Providers
    // ═══════════════════════════════════════════════════════════════
    const providers = ref([]);
    const providersLoading = ref(false);
    async function fetchProviders() {
        providersLoading.value = true;
        try {
            const { data } = await api.get('/api/providers', { headers: headers() });
            providers.value = data.providers;
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
        }
        finally {
            providersLoading.value = false;
        }
    }
    async function addProvider(payload) {
        try {
            await api.post('/api/providers', payload, { headers: headers() });
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
            throw e;
        }
    }
    /**
     * Delete a provider from the gateway. The server cascades the deletion
     * by removing any `RouteTarget` entries that reference the provider and
     * dropping routes that would otherwise end up with no targets.
     *
     * On success, refreshes both the provider and route lists so the UI
     * reflects the cascade.
     */
    async function deleteProvider(name) {
        try {
            await api.delete('/api/providers', {
                headers: headers(),
                data: { name },
            });
            // The server cascaded the deletion; refresh both lists so the UI
            // shows the updated routes (some may have been dropped entirely).
            await Promise.all([fetchProviders(), fetchRoutes()]);
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
            throw e;
        }
    }
    async function updateProvider(name, payload) {
        try {
            await api.put('/api/providers', { name, ...payload }, { headers: headers() });
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
            throw e;
        }
    }
    // ═══════════════════════════════════════════════════════════════
    // Routes
    // ═══════════════════════════════════════════════════════════════
    const routes = ref([]);
    const routesLoading = ref(false);
    async function fetchRoutes() {
        routesLoading.value = true;
        try {
            const { data } = await api.get('/api/routes', { headers: headers() });
            routes.value = data.routes;
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
        }
        finally {
            routesLoading.value = false;
        }
    }
    async function addRoute(providerName, payload) {
        try {
            await api.post(`/api/providers/${providerName}/routes`, payload, { headers: headers() });
        }
        catch (e) {
            if (e.response?.status === 401)
                handle401();
            throw e;
        }
    }
    // ═══════════════════════════════════════════════════════════════
    // Health
    // ═══════════════════════════════════════════════════════════════
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
    // ═══════════════════════════════════════════════════════════════
    // Analytics
    // ═══════════════════════════════════════════════════════════════
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
    // ═══════════════════════════════════════════════════════════════
    // Summary
    // ═══════════════════════════════════════════════════════════════
    const summary = computed(() => ({
        providerCount: providers.value.length,
        routeCount: routes.value.length,
        totalCalls: stats.value.reduce((s, r) => s + r.call_count, 0),
        totalErrors: stats.value.reduce((s, r) => s + r.error_count, 0),
    }));
    return {
        // Connection
        connection, validated, connecting, validating, connectError,
        isConnected, gatewayUrl,
        loadConnection, connect, disconnect, validateConnection,
        setRouter,
        // Providers
        providers, providersLoading, fetchProviders, addProvider, deleteProvider, updateProvider,
        // Routes
        routes, routesLoading, fetchRoutes, addRoute,
        // Health
        gatewayVersion, gatewayOnline, checkHealth,
        // Analytics
        stats, statsLoading, fetchStats,
        // Summary
        summary,
    };
});

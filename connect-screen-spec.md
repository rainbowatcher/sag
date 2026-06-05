# Spec: Gateway Connection Screen (Replace Sidebar API Key)

## Summary

Replace the sidebar API key input with a dedicated **Connect to Gateway** screen that
persists credentials in `localStorage` with a 30-day TTL. Auto-connect on revisit;
redirect to the connect screen when auth is missing or fails.

---

## Motivation

- The current sidebar API key input is too hidden and doesn't persist.
- User wants a first-class "connect" experience where credentials are remembered.

---

## User Profile

- **Audience**: single developer (just me).
- **Gateway location**: localhost during dev, but may be remote later.

---

## Functional Requirements

### 1. Dedicated Connect Screen (`/connect`)

- **Fields**:
  - **Gateway URL** (text input, default `http://127.0.0.1:3000`).
  - **API Key** (password input).
- **Connect button**: validates the connection before proceeding.
- If no saved connection exists on first visit, this screen is the app entry point.
- URL field is pre-filled from `localStorage` if previously saved.
- API key is *never* pre-filled (security — browser password managers handle it).

### 2. Connection Validation

- On "Connect" click: call `GET /health` via the gateway URL (proxied or direct).
  - If `200 OK` and `{ status: "ok" }`: save credentials, redirect to `/`.
  - If network error: show "Cannot reach gateway at <url>".
  - If non-200: show the error body message.
- Auth validity is also checked by calling `GET /api/providers` with the key.
  - If 401: show "Invalid API key — please check and re-enter."
  - If 200: connection is fully valid.

### 3. localStorage Persistence

- After successful connection, store:

  ```json
  {
    "gatewayUrl": "http://127.0.0.1:3000",
    "apiKey": "sag-test-key",
    "savedAt": 1717500000000
  }
  ```

  under the key `sag-connection`.

- **TTL**: 30 days from `savedAt`. On expiry, clear the entry and redirect to `/connect`.
- On explicit disconnect (sidebar button), clear the entry.

### 4. Auto-Connect on Reload

- On app startup (router `beforeEach`):
  1. Check `localStorage` for `sag-connection`.
  2. If missing or expired → redirect to `/connect`.
  3. If present: attempt a quick `GET /health` via the saved URL.
     - Success → proceed to the requested route.
     - Failure → clear connection, redirect to `/connect`.
- The connect screen itself is always accessible (no redirect loop).

### 5. Sidebar — Connection Status

- Remove the current API key input and `authError` banner from the sidebar.
- Show:
  - **Connection indicator**: green dot + "Connected" label.
  - **Gateway URL** in small text below (e.g., `http://127.0.0.1:3000`).
  - **Disconnect button**: clears `localStorage`, redirects to `/connect`.

### 6. Mid-Session Auth Failure (401)

- If any `/api/*` request returns 401 mid-session:
  - Clear `sag-connection` from `localStorage`.
  - Redirect to `/connect` with a query param `?reason=expired`.
  - Connect screen shows: "Your session has expired. Please reconnect."

### 7. Public Endpoints

- `/health` and `/v1/analytics/stats` remain public (no auth required). No change to backend.
- Only `/api/*` and `/v1/chat/completions` require auth.

---

## Technical Design

### New Files

| File | Purpose |
|---|---|
| `sag-ui/src/pages/ConnectPage.vue` | The connect screen component |
| `sag-ui/src/pages/ConnectPage.vue.js` | Compiled JS (auto-generated) |

### Modified Files

| File | Changes |
|---|---|
| `sag-ui/src/stores/api.ts` | Replace `apiKey` ref with `connection` object from localStorage; add `connect()`, `disconnect()`, `loadConnection()`, `isConnected` computed; update `baseURL` dynamically; keep `authError` for mid-session expiry |
| `sag-ui/src/stores/api.js` | Same changes in compiled JS |
| `sag-ui/src/router/index.ts` | Add `/connect` route; add `beforeEach` guard for auto-connect / redirect |
| `sag-ui/src/components/AppLayout.vue` | Remove API key input & auth banner; add connection status + disconnect button |

### Router Guard Logic

```ts
router.beforeEach(async (to) => {
  const store = useApiStore()

  // Allow access to /connect unconditionally
  if (to.path === '/connect') return true

  // Load saved connection from localStorage
  store.loadConnection()

  // No saved connection → redirect to connect
  if (!store.isConnected) return '/connect'

  // Auto-validate silently on first navigation
  if (!store.validated) {
    const ok = await store.validateConnection()
    if (!ok) return '/connect?reason=expired'
  }

  return true
})
```

### Store Changes (api.ts)

- Remove: `const apiKey = ref('')`
- Remove: `function headers()` (inline in axios config instead)
- Add:

  ```ts
  interface Connection {
    gatewayUrl: string
    apiKey: string
    savedAt: number
  }

  const TTL_MS = 30 * 24 * 60 * 60 * 1000 // 30 days

  const connection = ref<Connection | null>(null)
  const validated = ref(false)
  const isConnected = computed(() => connection.value !== null)

  function loadConnection() {
    const raw = localStorage.getItem('sag-connection')
    if (!raw) return
    const conn: Connection = JSON.parse(raw)
    if (Date.now() - conn.savedAt > TTL_MS) {
      localStorage.removeItem('sag-connection')
      return
    }
    connection.value = conn
    // Update axios baseURL dynamically
    api.defaults.baseURL = conn.gatewayUrl
  }

  function saveConnection(url: string, key: string) {
    const conn: Connection = { gatewayUrl: url, apiKey: key, savedAt: Date.now() }
    localStorage.setItem('sag-connection', JSON.stringify(conn))
    connection.value = conn
    api.defaults.baseURL = url
  }

  async function connect(url: string, key: string) {
    // 1. Health check
    const healthOk = await checkGatewayHealth(url)
    if (!healthOk) throw new Error('Cannot reach gateway')

    // 2. Auth check
    const authOk = await checkGatewayAuth(url, key)
    if (!authOk) throw new Error('Invalid API key')

    saveConnection(url, key)
    validated.value = true
  }

  function disconnect() {
    localStorage.removeItem('sag-connection')
    connection.value = null
    validated.value = false
    router.push('/connect')
  }
  ```

- Auth header building: create axios instance with interceptor that reads from `connection.value.apiKey`.

### ConnectPage.vue Design

- Centered card on a dark/neutral background.
- SAG logo/name at top.
- Two inputs: Gateway URL, API Key.
- Connect button with loading state.
- Error message area below the form.
- If `?reason=expired` in URL: show a banner above the form.

---

## Non-Goals

- Multi-profile / named gateway profiles (user said "just URL + key").
- Backend changes (auth mechanism stays the same).
- Changing which endpoints require auth.

---

## Edge Cases

| Scenario | Behavior |
|---|---|
| Saved URL is unreachable | Redirect to `/connect`, show connection error |
| API key becomes invalid mid-session | Clear storage, redirect to `/connect?reason=expired` |
| localStorage is full or disabled | Show a warning, fall back to in-memory only |
| User opens multiple tabs | Each tab validates independently (fine for single-user) |
| TTL expires while app is open | TTL only checked on `loadConnection()` (page load); mid-session expiry handled by 401 catch |

---

## Migration Path

1. User who previously had `apiKey` in memory will lose it on next reload.
2. First visit after deployment: redirected to `/connect`.
3. Enter `http://127.0.0.1:3000` + `sag-test-key` (from `gateway.toml`).
4. Key is now persisted for 30 days.

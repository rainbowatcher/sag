import axios from "axios"

// Step 1 (A2): the shared `api` instance no longer needs a `headers()`
// helper, because `connection.ts:saveConnection` keeps the global
// `api.defaults.headers.common.Authorization` in sync with the
// persisted apiKey. Every `api.X(...)` call now carries the Bearer
// header for free, including `/api/*` requests — see
// `scripts/lint-api-auth.mjs` for the build-time guard that warns
// when per-call headers re-appear (a regression of this contract).
export const api = axios.create({ baseURL: "" })

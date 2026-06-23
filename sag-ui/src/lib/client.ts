import axios from "axios"

// Shared axios instance. The Authorization header is kept in sync by
// `connection.ts`; see `scripts/lint-api-auth.mjs` for the guard that
// rejects per-call `headers:` on `/api/*` requests.
export const api = axios.create({ baseURL: "" })

-- Analytics M1: identity registry, two-level facts, and 1h rollup schema.
-- Stage 1 (identity registry + facts) and stage 3 (rollup) tables are created
-- together so a single `sqlx::migrate!` application builds the full schema.
--
-- Conventions:
--   * UUID v7 identities (route_id / target_id) are generated in Rust and
--     stored as TEXT; they never appear in config or API responses.
--   * All timestamps are INTEGER unix milliseconds (UTC).
--   * Facts are permanent: request_results / target_attempts rows are never
--     deleted; target_attempts.request_id uses ON DELETE RESTRICT.
--   * Identities are only deactivated (active = 0), never physically deleted.
--   * Foreign-key enforcement is the Rust connection config's job
--     (SqliteConnectOptions::foreign_keys); no connection-level PRAGMA
--     is issued from migrations.

-- -------------------------------------------------------------- identity --

CREATE TABLE IF NOT EXISTS analytics_routes (
    route_id    TEXT PRIMARY KEY,          -- UUID v7, DB-only identity
    route_name  TEXT NOT NULL UNIQUE,      -- stable business identity (= RouteConfig.model_name)
    active      INTEGER NOT NULL DEFAULT 1,
    first_seen  INTEGER NOT NULL,          -- unix ms
    last_seen   INTEGER NOT NULL           -- unix ms
);

CREATE TABLE IF NOT EXISTS analytics_route_targets (
    target_id   TEXT PRIMARY KEY,          -- UUID v7
    route_id    TEXT NOT NULL REFERENCES analytics_routes(route_id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,
    real_model  TEXT NOT NULL,             -- effective model (passthrough = requested model)
    active      INTEGER NOT NULL DEFAULT 1,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    UNIQUE (route_id, provider, real_model) -- target identity key
);

CREATE INDEX IF NOT EXISTS idx_route_targets_active
    ON analytics_route_targets (route_id, active);

-- ------------------------------------------------------------ facts (M1) --

-- One row per analyzed request (final result). Idempotent via
-- UNIQUE(request_id): a replayed event is a no-op, never a duplicate.
CREATE TABLE IF NOT EXISTS request_results (
    result_id             INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id            TEXT NOT NULL UNIQUE,   -- one row per request; idempotency key
    route_id              TEXT REFERENCES analytics_routes(route_id), -- NULL = no_route / client_error
    final_status          TEXT NOT NULL CHECK (final_status IN (
        'success', 'client_error', 'no_route', 'all_disabled',
        'model_failure', 'client_cancelled'
    )),
    failure_reason        TEXT,                   -- diagnostic detail, orthogonal to status
    requested_model       TEXT NOT NULL DEFAULT '',
    actual_model          TEXT,                   -- final successful path only; else NULL
    stream                INTEGER NOT NULL DEFAULT 0,
    started_at            INTEGER NOT NULL,       -- unix ms
    completed_at          INTEGER NOT NULL,       -- unix ms; hour-bucket key
    duration_ms           INTEGER NOT NULL,       -- = completed_at - started_at
    prompt_tokens         INTEGER,                -- only final_status = success non-NULL
    completion_tokens     INTEGER,
    total_tokens          INTEGER,
    cached_tokens         INTEGER,                -- NULL = unknown (distinct from miss)
    cache_creation_tokens INTEGER,
    ttft_ms               INTEGER,
    upstream_duration_ms  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_request_results_completed
    ON request_results (completed_at);
CREATE INDEX IF NOT EXISTS idx_request_results_route_completed
    ON request_results (route_id, completed_at);
CREATE INDEX IF NOT EXISTS idx_request_results_status
    ON request_results (final_status, completed_at);

-- One row per actual target attempt (final result of a retry sequence).
-- attempt status is success | failure | cancelled (client cancelled in-flight).
CREATE TABLE IF NOT EXISTS target_attempts (
    attempt_id            INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id            TEXT NOT NULL REFERENCES request_results(request_id) ON DELETE RESTRICT, -- facts are permanent: no delete/cascade
    target_id             TEXT NOT NULL REFERENCES analytics_route_targets(target_id),
    route_id              TEXT NOT NULL REFERENCES analytics_routes(route_id),
    actual_model          TEXT NOT NULL,          -- model actually used (passthrough = requested model)
    status                TEXT NOT NULL CHECK (status IN ('success', 'failure', 'cancelled')),
    failure_reason        TEXT,
    stream                INTEGER NOT NULL DEFAULT 0,
    started_at            INTEGER NOT NULL,
    completed_at          INTEGER NOT NULL,
    duration_ms           INTEGER NOT NULL,
    prompt_tokens         INTEGER,                -- only status = success non-NULL
    completion_tokens     INTEGER,
    total_tokens          INTEGER,
    cached_tokens         INTEGER,                -- NULL = unknown
    ttft_ms               INTEGER,
    upstream_duration_ms  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_target_attempts_request
    ON target_attempts (request_id);
CREATE INDEX IF NOT EXISTS idx_target_attempts_target_completed
    ON target_attempts (target_id, completed_at);
CREATE INDEX IF NOT EXISTS idx_target_attempts_route_completed
    ON target_attempts (route_id, completed_at);

-- call_logs keeps its droppable-diagnostics role; the new nullable identity
-- columns are filled for new writes only. Existing rows stay NULL, never
-- backfilled.
ALTER TABLE call_logs ADD COLUMN route_id TEXT;
ALTER TABLE call_logs ADD COLUMN target_id TEXT;

-- -------------------------------------------------------- rollup (M1) ----
-- Hour buckets are keyed by UTC hour start (unix ms). Rollup tables only
-- accelerate queries; fact tables are never derived or deleted from them.

-- Global per-hour aggregate across all routes.
CREATE TABLE IF NOT EXISTS rollup_global_hourly (
    hour                   INTEGER PRIMARY KEY,   -- hour start, unix ms (UTC)
    requests               INTEGER NOT NULL,      -- = success + client_error + no_route + all_disabled + model_failure + client_cancelled
    success                INTEGER NOT NULL,
    client_error           INTEGER NOT NULL,
    no_route               INTEGER NOT NULL,
    all_disabled           INTEGER NOT NULL,
    model_failure          INTEGER NOT NULL,
    client_cancelled       INTEGER NOT NULL,
    prompt_tokens          INTEGER NOT NULL,
    completion_tokens      INTEGER NOT NULL,
    total_tokens           INTEGER NOT NULL,
    cached_tokens          INTEGER NOT NULL,
    cache_creation_tokens  INTEGER NOT NULL,
    cached_known_count     INTEGER NOT NULL,      -- known-cache-info successful requests (hit-rate denominator)
    cached_hit_count       INTEGER NOT NULL,      -- of those, cached_tokens > 0
    prompt_tokens_cached_known INTEGER NOT NULL,  -- sum of prompt_tokens over known samples (token hit-rate denominator)
    e2e_count              INTEGER NOT NULL,      -- latency sample count (coverage denominator)
    e2e_sum_ms             INTEGER NOT NULL,
    ttft_count             INTEGER NOT NULL,
    ttft_sum_ms            INTEGER NOT NULL,
    upstream_count         INTEGER NOT NULL,
    upstream_sum_ms        INTEGER NOT NULL,
    CHECK (requests = success + client_error + no_route + all_disabled + model_failure + client_cancelled),
    CHECK (cached_known_count >= cached_hit_count)
);

-- Per-route per-hour aggregate (matched routes only; no_route / client_error
-- never appear here).
CREATE TABLE IF NOT EXISTS rollup_route_hourly (
    hour                   INTEGER NOT NULL,
    route_id               TEXT NOT NULL,
    requests               INTEGER NOT NULL,     -- = success + all_disabled + model_failure + client_cancelled
    success                INTEGER NOT NULL,
    model_failure          INTEGER NOT NULL,
    all_disabled           INTEGER NOT NULL,
    client_cancelled       INTEGER NOT NULL,
    prompt_tokens          INTEGER NOT NULL,
    completion_tokens      INTEGER NOT NULL,
    total_tokens           INTEGER NOT NULL,
    cached_tokens          INTEGER NOT NULL,
    cached_known_count     INTEGER NOT NULL,      -- known-cache-info successful requests (hit-rate denominator)
    cached_hit_count       INTEGER NOT NULL,      -- of those, cached_tokens > 0
    prompt_tokens_cached_known INTEGER NOT NULL,  -- sum of prompt_tokens over known samples (token hit-rate denominator)
    e2e_count              INTEGER NOT NULL,
    e2e_sum_ms             INTEGER NOT NULL,
    ttft_count             INTEGER NOT NULL,
    ttft_sum_ms            INTEGER NOT NULL,
    upstream_count         INTEGER NOT NULL,
    upstream_sum_ms        INTEGER NOT NULL,
    PRIMARY KEY (hour, route_id),
    CHECK (requests = success + all_disabled + model_failure + client_cancelled),
    CHECK (cached_known_count >= cached_hit_count)
);

-- Per-target per-hour aggregate of actual attempts.
CREATE TABLE IF NOT EXISTS rollup_target_hourly (
    hour                   INTEGER NOT NULL,
    target_id              TEXT NOT NULL,
    attempts               INTEGER NOT NULL,     -- = success + failure + cancelled
    success                INTEGER NOT NULL,
    failure                INTEGER NOT NULL,
    cancelled              INTEGER NOT NULL,     -- client-cancelled in-flight attempts
    prompt_tokens          INTEGER NOT NULL,
    completion_tokens      INTEGER NOT NULL,
    total_tokens           INTEGER NOT NULL,
    cached_tokens          INTEGER NOT NULL,
    cached_known_count     INTEGER NOT NULL,      -- known-cache-info successful attempts (hit-rate denominator)
    cached_hit_count       INTEGER NOT NULL,      -- of those, cached_tokens > 0
    prompt_tokens_cached_known INTEGER NOT NULL,  -- sum of prompt_tokens over known samples (token hit-rate denominator)
    e2e_count              INTEGER NOT NULL,
    e2e_sum_ms             INTEGER NOT NULL,
    ttft_count             INTEGER NOT NULL,
    ttft_sum_ms            INTEGER NOT NULL,
    upstream_count         INTEGER NOT NULL,
    upstream_sum_ms        INTEGER NOT NULL,
    PRIMARY KEY (hour, target_id),
    CHECK (attempts = success + failure + cancelled),
    CHECK (cached_known_count >= cached_hit_count)
);

-- Sparse log-scale latency histogram. bucket_idx: 0 = the 0ms bucket;
-- 1..N = log buckets (1.01x growth from 1ms); samples beyond the fixed
-- capacity go to the overflow bucket (convention: bucket_idx = -1);
-- NULL latency values never enter the histogram.
CREATE TABLE IF NOT EXISTS rollup_latency_hist (
    hour       INTEGER NOT NULL,
    scope      TEXT NOT NULL CHECK (scope IN ('route', 'target')),
    owner_id   TEXT NOT NULL,             -- route_id or target_id
    dim        TEXT NOT NULL CHECK (dim IN ('e2e', 'ttft', 'upstream')),
    bucket_idx INTEGER NOT NULL,          -- 0 = 0ms bucket; overflow = -1
    count      INTEGER NOT NULL,
    PRIMARY KEY (hour, scope, owner_id, dim, bucket_idx)
);

-- Hour-bucket state machine: pending | processing | completed | failed.
CREATE TABLE IF NOT EXISTS rollup_bucket_state (
    hour                 INTEGER PRIMARY KEY,
    state                TEXT NOT NULL CHECK (state IN ('pending', 'processing', 'completed', 'failed')),
    dirty                INTEGER NOT NULL DEFAULT 0, -- completed bucket received late fact -> 1
    attempt              INTEGER NOT NULL DEFAULT 0, -- idempotent retry counter
    processing_started_at INTEGER,                  -- timeout recovery detection
    updated_at           INTEGER NOT NULL
);

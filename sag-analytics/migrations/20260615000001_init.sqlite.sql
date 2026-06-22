-- Base schema for the analytics call_logs table.
-- Timestamp stored as INTEGER (unix milliseconds) for efficient range queries.

CREATE TABLE IF NOT EXISTS call_logs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id          TEXT    NOT NULL,
    provider            TEXT    NOT NULL DEFAULT '',
    model               TEXT    NOT NULL DEFAULT '',
    real_model          TEXT    NOT NULL DEFAULT '',
    stream              INTEGER NOT NULL DEFAULT 0,
    status_code         INTEGER NOT NULL DEFAULT 0,
    duration_ms         INTEGER NOT NULL DEFAULT 0,
    prompt_tokens       INTEGER,
    completion_tokens   INTEGER,
    total_tokens        INTEGER,
    error               TEXT,
    timestamp           INTEGER NOT NULL,

    -- Stage 0.4: Extended CallLog fields
    reasoning_effort       TEXT,
    upstream_duration_ms   INTEGER,
    ttft_ms                INTEGER,
    cached_tokens          INTEGER,
    cache_creation_tokens  INTEGER,
    reasoning_tokens       INTEGER
);

CREATE INDEX IF NOT EXISTS idx_call_logs_ts
    ON call_logs (timestamp);

CREATE INDEX IF NOT EXISTS idx_call_logs_provider_ts
    ON call_logs (provider, timestamp);

CREATE INDEX IF NOT EXISTS idx_call_logs_model_ts
    ON call_logs (model, timestamp);

-- Real-model queries are common (route aliases); the `(real_model, timestamp)`
-- composite index covers both lookups and time-bucketed scans introduced in
-- later stages.
CREATE INDEX IF NOT EXISTS idx_call_logs_real_model
    ON call_logs (real_model, timestamp);
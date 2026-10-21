-- Session affinity (P5): hashed session identity on the legacy call_logs
-- table. 32-hex-char `SessionKey::map_id` digest — the raw session value
-- never lands in the database. Nullable: session-less requests and
-- non-session_sticky routes stay NULL (never backfilled).
ALTER TABLE call_logs ADD COLUMN session_key TEXT;
CREATE INDEX IF NOT EXISTS idx_call_logs_session_key ON call_logs(session_key);

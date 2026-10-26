-- Session info card: durable, earliest-requester-wins metadata per hashed
-- session identity. `session_key` is the same 32-hex digest stored in
-- `call_logs.session_key` (see `SessionKey::map_id`); the remaining columns
-- hold the displayable side of that identity — the extraction source, the
-- (truncated) conversation id and a title-style preview of the session
-- opener. `first_seen` / `last_seen` are unix milliseconds.
--
-- The table is deliberately NOT part of the `call_logs` rotation: rotation
-- only renames `call_logs` and its `call_logs_arch_*` cold tables, so this
-- table keeps its own retention (default 7 days, `0` = keep forever) and its
-- own single index on `last_seen` for the prune sweep.
CREATE TABLE IF NOT EXISTS session_registry (
    session_key TEXT PRIMARY KEY,
    session_source TEXT NOT NULL,
    conversation_id TEXT,
    first_message_preview TEXT,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_registry_last_seen ON session_registry(last_seen);

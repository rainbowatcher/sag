CREATE INDEX IF NOT EXISTS idx_call_logs_status_ts
    ON call_logs (status_code, timestamp);

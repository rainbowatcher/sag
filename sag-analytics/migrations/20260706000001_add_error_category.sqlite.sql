-- Add machine-readable error category to call_logs.
-- NULL for successful calls; a snake_case category string for errors
-- (e.g. "model_not_found", "provider_error", "timeout", "stream_error").
ALTER TABLE call_logs ADD COLUMN error_category TEXT;

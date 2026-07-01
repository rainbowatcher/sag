-- Add client HTTP identification fields to call_logs.
-- Both columns allow NULL so existing rows remain compatible.
ALTER TABLE call_logs ADD COLUMN user_agent TEXT;
ALTER TABLE call_logs ADD COLUMN client_ip TEXT;

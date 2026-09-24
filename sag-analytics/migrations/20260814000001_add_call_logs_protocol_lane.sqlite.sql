-- M7: protocol / lane dimensions on the legacy call_logs table.
-- Nullable TEXT so pre-M7 rows stay NULL (never backfilled); matches
-- the same columns already added to request_results / target_attempts
-- in 20260813000001_add_protocol_lane.
ALTER TABLE call_logs ADD COLUMN ingress_protocol TEXT;
ALTER TABLE call_logs ADD COLUMN upstream_protocol TEXT;
ALTER TABLE call_logs ADD COLUMN lane TEXT;

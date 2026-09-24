-- M7: protocol / lane dimensions on the two formal fact tables.
-- All columns are nullable TEXT so pre-M7 rows stay NULL (never backfilled)
-- and the change is forward/backward compatible without data migration.
ALTER TABLE request_results ADD COLUMN ingress_protocol TEXT;
ALTER TABLE request_results ADD COLUMN lane TEXT;
ALTER TABLE target_attempts ADD COLUMN upstream_protocol TEXT;
ALTER TABLE target_attempts ADD COLUMN lane TEXT;

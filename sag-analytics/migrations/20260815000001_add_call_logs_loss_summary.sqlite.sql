-- M8: LossInventory summary on the legacy call_logs table.
-- Nullable TEXT so pre-M8 rows stay NULL (never backfilled); matches
-- the protocol/lane columns added in 20260814000001.
ALTER TABLE call_logs ADD COLUMN loss_summary TEXT;

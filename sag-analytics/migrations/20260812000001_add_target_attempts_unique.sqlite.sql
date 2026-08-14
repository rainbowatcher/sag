-- Analytics M1.2: DB-level idempotency backstop for target_attempts.
--
-- Application-level idempotency already prevents replayed events from
-- duplicating attempt rows (early-return on duplicate request_id plus
-- `ON CONFLICT(request_id) DO NOTHING` on request_results), but neither a
-- concurrent writer race nor an event whose targets vector repeats the same
-- (provider, real_model) tuple may ever produce two rows for the same
-- (request_id, target_id) pair: retries are attempts, not attempt rows.
-- This unique index makes the pair a hard invariant enforced by SQLite,
-- and is the conflict target for the writer's
-- `ON CONFLICT(request_id, target_id) DO NOTHING` insert.

CREATE UNIQUE INDEX IF NOT EXISTS uq_target_attempts_request_target
    ON target_attempts (request_id, target_id);

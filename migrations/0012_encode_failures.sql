-- migrations/0012_encode_failures.sql — record episodics whose encode failed to
-- land, for eyeballing + manual catch-up (CLA-132).
--
-- The pure-queue encode pipeline is event-driven: a capture submits a batch and
-- a delayed poll message dispatches the result. Hard failures are rare but real:
-- a batch errors/expires on Anthropic's side, a poll message exhausts its
-- attempts, or a single result comes back errored. When that happens the
-- episodic stays unintegrated (integrated = 0) — never lost — and we drop a row
-- here so a human can see WHICH episodics didn't make it and why, then run the
-- manual catch-up (POST /admin/encode-run) to re-gather and re-submit them.
--
-- This is the "table for now" backstop (vs an email digest): cheap, queryable,
-- no third-party send dependency to wire. No foreign key on episodic_id — same
-- rule as the other audit tables: a forgotten episodic must never block or mask
-- an insert here.
--
-- Query pattern:
--   SELECT failed_at, substr(episodic_id,1,8) AS ep, reason
--   FROM encode_failures ORDER BY failed_at DESC LIMIT 50;

CREATE TABLE IF NOT EXISTS encode_failures (
    failure_id   TEXT PRIMARY KEY,
    episodic_id  TEXT NOT NULL,
    batch_id     TEXT,            -- the Anthropic batch it was in, if any
    reason       TEXT NOT NULL,   -- 'batch_errored' | 'batch_expired' | 'poll_exhausted' | 'result_errored'
    failed_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_encode_failures_failed_at
    ON encode_failures(failed_at DESC);
CREATE INDEX IF NOT EXISTS idx_encode_failures_episodic
    ON encode_failures(episodic_id);

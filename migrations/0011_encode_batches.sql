-- migrations/0011_encode_batches.sql — track in-flight Anthropic Message Batches
-- for the async encode pipeline (CLA-132).
--
-- Background: the per-capture inline encode made one synchronous Haiku call per
-- episodic inside the queue consumer. A large compaction dump (~22k chars) takes
-- minutes to judge + dispatch, overran the consumer's wall-time budget, and left
-- the episodic unintegrated with no retry (crons were empty). See CLA-132 and the
-- 2026-06-10 cbe96edc strand: the cognition was fine, the execution context
-- couldn't finish a slow single-shot call.
--
-- The fix moves encode onto Anthropic's Message Batches API. A cron submits the
-- unintegrated episodics as one batch (custom_id = episodic_id); Anthropic
-- processes them off our clock at 50% cost; a poll cron fetches the results and
-- dispatches them through the same `dispatch_one` primitives. No worker ever
-- holds the inference, so nothing times out — and a dropped result just gets
-- re-gathered next tick instead of stranding.
--
-- This table is the in-flight ledger: which batches are open, what they hold, how
-- far each has progressed. No foreign key on episodic ids — an episodic could be
-- forgotten between submit and dispatch, and a ledger insert must never mask or
-- block that (same rule as 0010's memory_revisions).
--
-- State machine (status):
--   'in_progress' — submitted to Anthropic, not yet ended
--   'dispatched'  — results fetched, decisions applied, episodics marked integrated
--   'failed'      — batch errored/expired; its episodics stay unintegrated and are
--                   re-gathered by the next submit tick
--
-- Query patterns:
--   -- open batches to poll
--   SELECT batch_id, episodic_ids FROM encode_batches WHERE status = 'in_progress';
--   -- recent throughput
--   SELECT status, COUNT(*) AS n FROM encode_batches GROUP BY status;

CREATE TABLE IF NOT EXISTS encode_batches (
    batch_id      TEXT PRIMARY KEY,   -- Anthropic msgbatch_... id
    status        TEXT NOT NULL,      -- 'in_progress' | 'dispatched' | 'failed'
    -- JSON array of episodic ids in this batch (custom_id = episodic_id). Used to
    -- exclude in-flight episodics from the next submit, and to reconcile results.
    episodic_ids  TEXT NOT NULL,
    submitted_at  TEXT NOT NULL,
    polled_at     TEXT,
    poll_count    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_encode_batches_status
    ON encode_batches(status);
CREATE INDEX IF NOT EXISTS idx_encode_batches_submitted_at
    ON encode_batches(submitted_at DESC);

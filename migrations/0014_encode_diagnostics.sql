-- migrations/0014_encode_diagnostics.sql — forensic capture for the encode whiff.
--
-- The failure is asynchronous and looks identical to success: a clean, valid,
-- EMPTY decisions array. We can't catch it live in `wrangler tail`, and a rate
-- across runs diagnoses nothing — reliability is a cause problem. So this table
-- autopsies each judge call: what the model actually returned (stop_reason, usage,
-- whether a tool_use block was even present, the content-block types), the
-- episodic's size, and how many decisions came back — for both the queue/batch
-- path (where a first-submission fails) and the endpoint/sync path (where a
-- re-run succeeds). One failing batch row beside one succeeding sync row for the
-- same episodic is the whole answer.
CREATE TABLE IF NOT EXISTS encode_diagnostics (
    diag_id         TEXT PRIMARY KEY,
    episodic_id     TEXT NOT NULL,
    source          TEXT,            -- 'queue-batch' | 'endpoint-sync'
    candidate_count INTEGER,         -- -1 = not available on this path
    content_chars   INTEGER,
    http_status     INTEGER,
    stop_reason     TEXT,
    usage_in        INTEGER,
    usage_out       INTEGER,
    has_tool_use    INTEGER,         -- 1/0
    content_types   TEXT,            -- e.g. 'text,tool_use'  or  'text' (no tool call)
    decision_count  INTEGER,         -- -1 = parse failed (no tool_use / bad input)
    note            TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_encode_diag_episodic ON encode_diagnostics(episodic_id);
CREATE INDEX IF NOT EXISTS idx_encode_diag_created  ON encode_diagnostics(created_at);

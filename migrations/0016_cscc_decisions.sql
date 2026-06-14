-- migrations/0016_cscc_decisions.sql — the CSCC merge decision cache (CLA-134).
--
-- CSCC re-clusters the whole semantic layer each run. MERGE verdicts self-
-- eliminate (folded members supersede and drop out of the live set), but
-- KEEP_DISTINCT clusters stay live and re-form every run — so without a cache the
-- judge re-pays a Haiku call on every settled "these are distinct" cluster,
-- forever. This caches each cluster's verdict so a settled cluster is decided once.
--
-- The cache also makes dry-run → commit DETERMINISTIC: the dry-run judges and
-- stores the verdict (keeper included); the commit reads that same row, so it
-- folds exactly what was previewed instead of re-judging with a fresh, stochastic
-- call (which is what flipped a keeper between runs during calibration).
--
-- Invalidation is by THREE keys, not membership alone:
--   cluster_hash = SHA-256(sorted member UUIDs)    — set changes  → new row / re-judge
--   content_hash = SHA-256(sorted member contents) — a member reframed → re-judge
--   policy_ver   = the merge-judge prompt version   — prompt tuned  → re-judge all
-- (cluster_decisions, migration 0004, invalidates on member-set only — wrong for
-- CSCC, where a content edit or a prompt nudge genuinely changes the right answer.)
--
-- FK-free deliberately: cluster_hash is content-derived and members may be
-- superseded, so an FK into `memories` would break on the very folds this serves.

CREATE TABLE IF NOT EXISTS cscc_decisions (
    cluster_hash  TEXT PRIMARY KEY,
    content_hash  TEXT NOT NULL,
    policy_ver    INTEGER NOT NULL,
    action        TEXT NOT NULL,       -- 'merge' | 'keep_distinct'
    keeper_id     TEXT,                -- merge only
    decided_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cscc_decisions_action ON cscc_decisions(action);

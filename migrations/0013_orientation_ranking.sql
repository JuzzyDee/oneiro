-- migrations/0013_orientation_ranking.sql — columns for the Stage-C Whittling
-- (CLA-131 / CLA-126). Stage B keeps each axis tight; Stage C keeps the SET to
-- ~20 by ranking and deactivating, so the two stages decouple cleanly: per-axis
-- size vs whole-set count.
--
-- The cut is a PURE QUERY — deterministic, auditable, un-poisonable (no model in
-- the loop). It ranks orientation by Recency × Frequency × Meaning, pins the
-- cores, keeps the top ~20 ACTIVE, and deactivates the rest. Deactivation is not
-- demotion or deletion: a dormant axis stays an orientation row, kept and
-- reactivatable the instant its R/F/M climbs again — so "never lose orientation"
-- holds by construction.
--
--   R (recency)   — computed live: time since last touch (already maintained).
--   F (frequency) — computed live: inbound semantic_orientation edge count.
--   M (meaning)   — the substitution that makes RFM ours: not Monetary but
--                   importance-for-relating, a model-supplied 1–10 score rated
--                   ONCE when the axis is synthesised (cheap, stateless, no
--                   ratchet) and stored here.
--
-- Columns:
--   core       — hard pin: never deactivated regardless of R/F/M. Precious few (1–2).
--   meaning    — the M. NULL until first rated (existing axes get a one-off backfill).
--   is_active  — surfaced at recall / session-start? The Whittling's only output.
--                Default 1 so nothing vanishes on deploy; the Whittling sets it.

ALTER TABLE memories ADD COLUMN core       INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memories ADD COLUMN meaning    INTEGER;
ALTER TABLE memories ADD COLUMN is_active  INTEGER NOT NULL DEFAULT 1;

-- Recall's hot path is "orientation rows that are active"; index it.
CREATE INDEX IF NOT EXISTS idx_memories_orientation_active
    ON memories(memory_type, is_active);

-- Seed the core pins from existing intent, not a hand-picked list: the precious
-- few axes already carry a "core" tag (the relationship pillar, the philosophy
-- pillar). Promote that tag to the column so the Whittling never deactivates
-- them. tags is a JSON array string (e.g. ["core","relationship"]), so the
-- quoted-token match can't catch a substring like "hardcore".
UPDATE memories SET core = 1
    WHERE memory_type = 'orientation' AND tags LIKE '%"core"%';

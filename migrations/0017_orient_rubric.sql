-- migrations/0017_orient_rubric.sql — the familiarity rubric's scored dimensions
-- (CLA-125, Phase 2b). RFM was the wrong instrument for orientation ranking:
-- recency is a recall-relevance signal and churns a STABILITY layer toward
-- recently-edged work, evicting stable relational bedrock (it evicted the
-- name-decline and the crisis axis); and the single `meaning` scalar saturated
-- (~everything 9), so it couldn't discriminate within the relational set.
--
-- The Whittle now ranks by the SUM of four 0-3 dimensions the Sonnet synthesis
-- judge scores per axis — "what makes the room the room," not "what's been active
-- lately." The two binary GATES (unregenerable, describe-not-prescribe) are
-- enforced at create time (Phase 2a) and need no storage.
--
--   orient_relational    — relational-load-bearing: stranger-vs-under-informed
--   orient_durability    — still true and relevant in months (the inverse of recency)
--   orient_irreplaceable — knowable only across many conversations, not from one
--   orient_cost          — cost-of-getting-it-wrong: the landmine / asymmetric downside
--
-- NULL until rated; existing axes get a one-off Sonnet backfill (as `meaning` did).
-- `meaning` is retained for history but no longer drives the cut.

ALTER TABLE memories ADD COLUMN orient_relational    INTEGER;
ALTER TABLE memories ADD COLUMN orient_durability    INTEGER;
ALTER TABLE memories ADD COLUMN orient_irreplaceable INTEGER;
ALTER TABLE memories ADD COLUMN orient_cost          INTEGER;

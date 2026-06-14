-- migrations/0015_cv_latest_semantic.sql — the "recent distilled delta" view (CLA-136).
--
-- recall_orient's "what's happened lately" surface. V1 surfaced raw recent
-- episodics; in V2 an episodic is pipeline INPUT — decomposed into atomic
-- semantics — not surfacing material. A raw compaction summary in that slot is
-- a 10–25k-char context-transfer artifact, the opposite of the lightweight
-- glance recall is meant to be. So surface the SEMANTICS the latest capture
-- decomposed into instead: small, distilled, fresh. recall reads `summary`
-- (never `content`) from this view, ordered by lineage weight, capped — so a
-- big session's 0.5 "mention" units shed before its primary contributions.
--
-- Prototyped by hand in D1 to find the shape; promoted to IaC so every deploy
-- gets it. DROP-then-CREATE is idempotent and replaces any hand-rolled copy
-- with this canonical (weight-exposing, superseded-filtered) form.
DROP VIEW IF EXISTS cv_latest_semantic;
CREATE VIEW cv_latest_semantic AS
WITH latest_episodic AS (
    SELECT id FROM memories
    WHERE memory_type = 'episodic'
    ORDER BY created_at DESC
    LIMIT 1
)
SELECT sm.*, cl.weight AS lineage_weight
FROM consolidation_lineage cl
JOIN latest_episodic le ON cl.source_id = le.id
JOIN memories sm ON cl.parent_id = sm.id
WHERE cl.edge_type = 'episodic_semantic'
  AND sm.superseded = 0;
